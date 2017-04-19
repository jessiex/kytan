// Copyright 2016-2017 Chang Lan
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use serde_derive;
use std::net::{SocketAddr, IpAddr, Ipv4Addr, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering, ATOMIC_BOOL_INIT};
use std::io::{Write, Read};
use mio;
use dns_lookup;
use bincode::Infinite;
use bincode::serialize as encode;
use bincode::deserialize as decode;
use device;
use utils;
use snap;
use rand::{thread_rng, Rng};
use transient_hashmap::TransientHashMap;

pub static INTERRUPTED: AtomicBool = ATOMIC_BOOL_INIT;

type Id = u8;
type Token = u64;

#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum Message {
    Request,
    Response { id: Id, token: Token },
    Data { id: Id, token: Token, data: Vec<u8> },
}

const TUN: mio::Token = mio::Token(0);
const SOCK: mio::Token = mio::Token(1);

fn resolve(host: &str) -> Result<IpAddr, String> {
    let mut ip_list = try!(dns_lookup::lookup_host(host).map_err(|_| "dns_lookup::lookup_host"));
    let ip = ip_list.next().unwrap().unwrap();
    Ok(ip)
}

fn create_tun_attempt() -> device::Tun {
    fn attempt(id: u8) -> device::Tun {
        match id {
            255 => panic!("Unable to create TUN device."),
            _ => {
                match device::Tun::create(id) {
                    Ok(tun) => tun,
                    Err(_) => attempt(id + 1),
                }
            }
        }
    }
    attempt(0)
}

fn initiate(socket: &UdpSocket, addr: &SocketAddr) -> Result<(Id, Token), String> {
    let req_msg = Message::Request;
    let encoded_req_msg: Vec<u8> = try!(encode(&req_msg, Infinite)
        .map_err(|e| e.to_string()));

    let mut remaining_len = encoded_req_msg.len();
    while remaining_len > 0 {
        let sent_bytes = try!(socket.send_to(&encoded_req_msg, addr)
            .map_err(|e| e.to_string()));
        remaining_len -= sent_bytes;
    }
    info!("Request sent to {}.", addr);

    let mut buf = [0u8; 1600];
    let (len, recv_addr) = try!(socket.recv_from(&mut buf).map_err(|e| e.to_string()));
    assert_eq!(&recv_addr, addr);
    info!("Response received from {}.", addr);

    let resp_msg: Message = try!(decode(&buf[0..len]).map_err(|e| e.to_string()));
    match resp_msg {
        Message::Response { id, token } => Ok((id, token)),
        _ => Err(format!("Invalid message {:?} from {}", resp_msg, addr)),
    }
}


pub fn connect(host: &str, port: u16, default: bool) {
    info!("Working in client mode.");
    let remote_ip = resolve(host).unwrap();
    let remote_addr = SocketAddr::new(remote_ip, port);
    info!("Remote server: {}", remote_addr);

    let local_addr: SocketAddr = "0.0.0.0:0".parse::<SocketAddr>().unwrap();
    let socket = UdpSocket::bind(&local_addr).unwrap();

    let (id, token) = initiate(&socket, &remote_addr).unwrap();
    info!("Session established with token {}. Assigned IP address: 10.10.10.{}.",
          token,
          id);

    info!("Bringing up TUN device.");
    let mut tun = create_tun_attempt();
    let tun_rawfd = tun.as_raw_fd();
    tun.up(id);
    let tunfd = mio::unix::EventedFd(&tun_rawfd);
    info!("TUN device {} initialized. Internal IP: 10.10.10.{}/24.",
          tun.name(),
          id);

    let poll = mio::Poll::new().unwrap();
    info!("Setting up TUN device for polling.");
    poll.register(&tunfd, TUN, mio::Ready::readable(), mio::PollOpt::level()).unwrap();

    info!("Setting up socket for polling.");
    let sockfd = mio::udp::UdpSocket::from_socket(socket).unwrap();
    poll.register(&sockfd, SOCK, mio::Ready::readable(), mio::PollOpt::level()).unwrap();

    let mut events = mio::Events::with_capacity(1024);
    let mut buf = [0u8; 1600];

    // RAII so ignore unused variable warning
    let _gw = if default {
        Some(utils::DefaultGateway::create("10.10.10.1", &format!("{}", remote_addr.ip())))
    } else {
        None
    };

    let mut encoder = snap::Encoder::new();
    let mut decoder = snap::Decoder::new();

    info!("Ready for transmission.");

    loop {
        if INTERRUPTED.load(Ordering::Relaxed) {
            break;
        }

        poll.poll(&mut events, None).unwrap();

        for event in events.iter() {
            match event.token() {
                SOCK => {
                    let (len, addr) = sockfd.recv_from(&mut buf).unwrap().unwrap();
                    let msg: Message = decode(&buf[0..len]).unwrap();
                    match msg {
                        Message::Request |
                        Message::Response { id: _, token: _ } => {
                            warn!("Invalid message {:?} from {}", msg, addr);
                        }
                        Message::Data { id: _, token: server_token, data } => {
                            if token == server_token {
                                let decompressed_data = decoder.decompress_vec(&data).unwrap();
                                let data_len = decompressed_data.len();
                                let mut sent_len = 0;
                                while sent_len < data_len {
                                    sent_len += tun.write(&decompressed_data[sent_len..data_len])
                                        .unwrap();
                                }
                            } else {
                                warn!("Token mismatched. Received: {}. Expected: {}",
                                      server_token,
                                      token);
                            }
                        }
                    }
                }
                TUN => {
                    let len: usize = tun.read(&mut buf).unwrap();
                    let data = &buf[0..len];
                    let msg = Message::Data {
                        id: id,
                        token: token,
                        data: encoder.compress_vec(data).unwrap(),
                    };
                    let encoded_msg = encode(&msg, Infinite).unwrap();
                    let data_len = encoded_msg.len();
                    let mut sent_len = 0;
                    while sent_len < data_len {
                        sent_len += sockfd.send_to(&encoded_msg[sent_len..data_len], &remote_addr)
                            .unwrap()
                            .unwrap();
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

pub fn serve(port: u16) {
    if cfg!(not(target_os = "linux")) {
        panic!("Server mode is only available in Linux!");
    }
    info!("Working in server mode.");

    info!("Enabling kernel's IPv4 forwarding.");
    utils::enable_ipv4_forwarding().unwrap();

    info!("Bringing up TUN device.");
    let mut tun = create_tun_attempt();
    tun.up(1);

    let tun_rawfd = tun.as_raw_fd();
    let tunfd = mio::unix::EventedFd(&tun_rawfd);
    info!("TUN device {} initialized. Internal IP: 10.10.10.1/24.",
          tun.name());

    let addr = format!("0.0.0.0:{}", port).parse().unwrap();
    let sockfd = mio::udp::UdpSocket::bind(&addr).unwrap();
    info!("Listening on: 0.0.0.0:{}.", port);

    let poll = mio::Poll::new().unwrap();
    poll.register(&sockfd, SOCK, mio::Ready::readable(), mio::PollOpt::level()).unwrap();
    poll.register(&tunfd, TUN, mio::Ready::readable(), mio::PollOpt::level()).unwrap();

    let mut events = mio::Events::with_capacity(1024);

    let mut rng = thread_rng();
    let mut available_ids: Vec<Id> = (2..254).collect();
    let mut client_info: TransientHashMap<Id, (Token, SocketAddr)> = TransientHashMap::new(60);

    let mut buf = [0u8; 1600];
    let mut encoder = snap::Encoder::new();
    let mut decoder = snap::Decoder::new();
    info!("Ready for transmission.");

    loop {
        if INTERRUPTED.load(Ordering::Relaxed) {
            break;
        }

        // Clear expired client info
        available_ids.append(&mut client_info.prune());

        poll.poll(&mut events, None).unwrap();

        for event in events.iter() {
            match event.token() {
                SOCK => {
                    let (len, addr) = sockfd.recv_from(&mut buf).unwrap().unwrap();
                    let msg: Message = decode(&buf[0..len]).unwrap();
                    match msg {
                        Message::Request => {
                            let client_id: Id = available_ids.pop().unwrap();
                            let client_token: Token = rng.gen::<Token>();

                            client_info.insert(client_id, (client_token, addr));

                            info!("Got request from {}. Assigning IP address: 10.10.10.{}.",
                                  addr,
                                  client_id);

                            let reply = Message::Response {
                                id: client_id,
                                token: client_token,
                            };
                            let encoded_reply = encode(&reply, Infinite).unwrap();
                            let data_len = encoded_reply.len();
                            let mut sent_len = 0;
                            while sent_len < data_len {
                                sent_len +=
                                    sockfd.send_to(&encoded_reply[sent_len..data_len], &addr)
                                        .unwrap()
                                        .unwrap();
                            }
                        }
                        Message::Response { id: _, token: _ } => {
                            warn!("Invalid message {:?} from {}", msg, addr)
                        }
                        Message::Data { id, token, data } => {
                            match client_info.get(&id) {
                                None => warn!("Unknown data with token {} from id {}.", token, id),
                                Some(&(t, _)) => {
                                    if t != token {
                                        warn!("Unknown data with mismatched token {} from id {}. \
                                               Expected: {}",
                                              token,
                                              id,
                                              t);
                                    } else {
                                        let decompressed_data = decoder.decompress_vec(&data)
                                            .unwrap();
                                        let data_len = decompressed_data.len();
                                        let mut sent_len = 0;
                                        while sent_len < data_len {
                                            sent_len +=
                                                tun.write(&decompressed_data[sent_len..data_len])
                                                    .unwrap();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                TUN => {
                    let len: usize = tun.read(&mut buf).unwrap();
                    let data = &buf[0..len];
                    let client_id: u8 = data[19];

                    match client_info.get(&client_id) {
                        None => warn!("Unknown IP packet from TUN for client {}.", client_id),
                        Some(&(token, addr)) => {
                            let msg = Message::Data {
                                id: client_id,
                                token: token,
                                data: encoder.compress_vec(data).unwrap(),
                            };
                            let encoded_msg = encode(&msg, Infinite).unwrap();
                            sockfd.send_to(&encoded_msg, &addr).unwrap().unwrap();
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn resolve_test() {
    assert_eq!(resolve("127.0.0.1").unwrap(),
               IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
}

#[test]
fn create_tun_attempt_test() {
    create_tun_attempt();
}
