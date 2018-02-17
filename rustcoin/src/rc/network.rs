use rc::constants;
use rc::filesystem::{fetch_blocks, write_blocks};
use rc::blockdata::{Block, Address, Transaction};
use rc::blockdata::{MerkleRoot};
use rc::encode::Encodable;
use rc::util::{current_epoch, sha_256_bytes};
use rc::util;

use std::collections::HashMap;
use std::io;
use std::net::ToSocketAddrs;
use std::{env, net, thread, time};
use std::sync::mpsc;

pub fn start_node() {
    let port = match env::var("PORT") {
        Ok(port) => port,
        Err(_) => "8333".to_string(),
    };

    // let known_node = "rustcoin:8333";
    let known_node = "127.0.0.1:8333";

    println!("{}: {}", port, "Fetching blocks");
    let mut blocks = fetch_blocks();
    if blocks.len() == 0 {
        println!("{}: {}", port, "No blocks found, writing genesis block.");
        blocks.push(Block::genesis_block());
        write_blocks(&blocks);
    }
    let last_hash = blocks[blocks.len() - 1].hash();

    let mut active_nodes: HashMap<net::SocketAddr, u64> = HashMap::new();
    if port != "8333".to_string() {
        {
            active_nodes.insert(
                known_node.to_socket_addrs().unwrap().next().unwrap(),
                0,
            );
        }
    }

    let socket = net::UdpSocket::bind(format!("0.0.0.0:{}", &port)).unwrap();
    let duration: Option<time::Duration> = Some(time::Duration::new(0, 1000));
    socket.set_read_timeout(duration).unwrap();

    println!("Broadcasting on {}", port);
    let ping = Message {
        payload: Vec::new(),
        command: [0x70, 0x6f, 0x6e, 0x67, 0, 0, 0, 0, 0, 0, 0, 0],
    };
    let getaddr = Message {
        payload: Vec::new(),
        command: constants::GETADDR_CMD,
    };
    let mut getblocks = Message {
        payload: last_hash.to_vec(),
        command: constants::GETBLOCKS_CMD,
    };

    // mine
    let (block_snd, block_rcv) = mpsc::channel::<Block>();
    // let (tx_snd, tx_rcv) = mpsc::channel::<Transaction>();
    let (_prev_block_snd, prev_block_rcv) = mpsc::channel::<PrevBlock>();
    thread::spawn(move || {
        loop {
            let prev_block = prev_block_rcv.recv().unwrap();
            let mut last_ts_reset = time::Instant::now();
            let address = Address::new();
            let transaction = Transaction::create_coinbase_transaction(address.pk);
            let transactions = vec![transaction];
            let mut block = Block {
                index: prev_block.index + 1,
                version: [0; 2],
                merkle_root: transactions.merkle_root(),
                prev_hash: prev_block.hash,
                transactions: transactions,
                nonce: 0,
                timestamp: current_epoch(),
            };
            loop {
                // check if there's a new block
                let hash = block.hash();
                if util::hash_is_valid_with_current_difficulty(hash) {
                    println!("{:?}", block.serialize());
                    block_snd.send(block).unwrap();
                    break
                }
                
                if last_ts_reset.elapsed().as_secs() > 10 {
                    println!("{:?}", block.nonce);
                    block.timestamp = current_epoch();
                    block.nonce = 0;
                    last_ts_reset = time::Instant::now();
                } else {
                    block.nonce += 1;
                }
                match prev_block_rcv.try_recv() {
                    Ok(prev_block) => {
                        block.prev_hash = prev_block.hash;
                        block.nonce = 0;
                        block.index = prev_block.index + 1;
                    }
                    Err(err) => {
                        match err {
                            mpsc::TryRecvError::Empty => {}
                            mpsc::TryRecvError::Disconnected => {
                                println!("last_block_hash {:?}", err); // panic?
                            }
                        }
                    }
                }
            }            
        }
    });

    // ask all active nodes for addresses
    for (node, _) in active_nodes.iter() {
        socket.send_to(&getaddr.serialize(), &node).unwrap();
        println!("{}: Sent getaddr to {}", port, &node);
    }

    // Message receiver
    let mut stage = 1usize;
    // 1 get nodes
    // 2 get blockchain
    // 3 start mining latest block
    let start_time = time::Instant::now();
    let mut last_sent_pings = start_time;
    let mut last_sent_getblocks = start_time;

    loop {
        // 2mb, largest message size
        let mut buf = [0; 2_000_000];
        match socket.recv_from(&mut buf) {
            Ok((amt, src)) => {
                let mut message =
                    Message::deserialize(&mut buf[..amt].to_vec());
                active_nodes.insert(src, current_epoch());
                stage = match_message(
                    src,
                    &socket,
                    &mut message,
                    &mut active_nodes,
                    &blocks,
                    &port,
                    stage,
                );
            }
            Err(err) => {
                match err.kind() {
                    io::ErrorKind::WouldBlock => {
                        // good
                    }
                    _ => {
                        println!("socket {:?}", err);
                    }
                }
            }
        }

        // check if there's a new mined block
        match block_rcv.try_recv() {
            Ok(block) => {
                // got a new mined block
                let mut inv = Inv{
                    inv_vectors: Vec::new(),
                };
                inv.inv_vectors.push(InvVector {
                    kind: 1,
                    hash: block.hash(),
                });
                blocks.push(block);
                for (node, _) in active_nodes.iter() {
                    println!("{}: Sent new block to {}", port, &node);
                    socket.send_to(&inv.serialize(), node).unwrap();
                }
            }
            Err(err) => {
                match err {
                    mpsc::TryRecvError::Empty => {}
                    mpsc::TryRecvError::Disconnected => {
                        println!("channel {:?}", err); // panic?
                    }
                }
            }
        }

        if stage == 2usize && last_sent_getblocks.elapsed().as_secs() > 10 {
            let last_hash = blocks[blocks.len() - 1].hash();
            getblocks.payload = last_hash.to_vec();
            for (node, _) in active_nodes.iter() {
                println!("{}: Sent getblocks to {}", port, &node);
                socket.send_to(&getblocks.serialize(), node).unwrap();
            }
            last_sent_getblocks = time::Instant::now();
        }
        if last_sent_pings.elapsed().as_secs() > 10 {
            if stage == 1usize {
                for (node, _) in active_nodes.iter() {
                    socket.send_to(&getaddr.serialize(), &node).unwrap();
                    println!("{}: Sent getaddr to {}", port, &node);
                }
            }
            for (node, _) in active_nodes.iter() {
                println!("{}: Sent ping to {}", port, &node);
                socket.send_to(&ping.serialize(), node).unwrap();
            }
            last_sent_pings = time::Instant::now();
        }
    }
}

fn match_message(
    src: net::SocketAddr,
    socket: &net::UdpSocket,
    message: &mut Message,
    active_nodes: &mut HashMap<net::SocketAddr, u64>,
    blocks: &Vec<Block>,
    port: &String,
    stage: usize,
) -> usize {
    let command = String::from_utf8_lossy(&message.command);
    println!("{}: Command \"{}\" from {}", port, command, src);
    let pong = Message {
        payload: Vec::new(),
        command: [0x70, 0x6f, 0x6e, 0x67, 0, 0, 0, 0, 0, 0, 0, 0],
    };
    let mut stage = stage;
    match command.as_ref() {
        "ping\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}" => {
            socket.send_to(&pong.serialize(), src).unwrap();
        }
        "getaddr\u{0}\u{0}\u{0}\u{0}\u{0}" => {
            let addr = Addr::serialize(&active_nodes);
            socket.send_to(&addr.serialize(), src).unwrap();
        }
        "addr\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}" => {
            let addresses = Addr::deserialize(&mut message.payload);
            for address in &addresses {
                // Don't add me
                if socket.local_addr().unwrap() != address.address {
                    // TODO: take the freshest timestamp
                    println!(
                        "{}: Adding node address {}",
                        port, address.address
                    );
                    active_nodes.insert(address.address, address.ts);
                    if active_nodes.len() >= 3 {
                        stage = 2usize;
                    }
                }
            }
        }
        "getblocks\u{0}\u{0}\u{0}" => {
            let mut last_hash = [0; 32];
            last_hash[..].clone_from_slice(&message.payload[0..32]);
            let mut inv: Inv = Inv {
                inv_vectors: Vec::new(),
            };
            for n in (0..blocks.len()).rev() {
                let hash = blocks[n].hash();
                if last_hash == hash {
                    break;
                }
                inv.inv_vectors.push(InvVector {
                    kind: 1,
                    hash: hash,
                })
            }
            if inv.inv_vectors.len() > 0 {
                socket.send_to(&inv.serialize(), src).unwrap();
            }
            // referse for
        }
        "inv\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}" => {
            let _inv = Inv::deserialize(&mut message.payload);
            println!("{}: {}", port, "got new inv");
        }
        "pong\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}" => {}
        &_ => {
            println!("{}: No match for command {}", port, command);
        }
    }
    stage
}

struct PrevBlock {
    hash: [u8; 32],
    index: u32,
}

struct Message {
    command: [u8; 12],
    payload: Vec<u8>,
}

struct Addr {
    address: net::SocketAddr,
    ts: u64,
}

impl Addr {
    fn serialize(active_nodes: &HashMap<net::SocketAddr, u64>) -> Message {
        let addr_ascii = [0x61, 0x64, 0x64, 0x72, 0, 0, 0, 0, 0, 0, 0, 0];
        let addrs: Vec<u8> = Vec::new();
        let mut addr = Message {
            command: addr_ascii,
            payload: addrs,
        };
        (active_nodes.len() as u16).serialize(&mut addr.payload);
        for (sock, ts) in active_nodes {
            let ip_octets = match sock.ip() {
                net::IpAddr::V4(ip) => ip.octets(),
                net::IpAddr::V6(_) => continue,
            };
            addr.payload.extend_from_slice(&ip_octets);
            sock.port().serialize(&mut addr.payload);
            ts.serialize(&mut addr.payload);
        }
        addr
    }
    fn deserialize(payload: &mut Vec<u8>) -> Vec<Addr> {
        let len: u16 = Encodable::deserialize(payload);
        let mut addresses: Vec<Addr> = Vec::new();
        for _ in 0..len {
            let ip: [u8; 4] = Encodable::deserialize(payload);
            let port: u16 = Encodable::deserialize(payload);
            let ts: u64 = Encodable::deserialize(payload);
            let ipv4 = net::Ipv4Addr::new(ip[0], ip[2], ip[2], ip[3]);
            let address = net::SocketAddr::new(net::IpAddr::V4(ipv4), port);
            addresses.push(Addr {
                address: address,
                ts: ts,
            });
        }
        addresses
    }
}

struct Inv {
    inv_vectors: Vec<InvVector>,
}

impl Inv {
    fn serialize(&self) -> Vec<u8> {
        let inv_ascii = [0x69, 0x6e, 0x76, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut payload: Vec<u8> = Vec::new();
        (self.inv_vectors.len() as u32).serialize(&mut payload);
        for invv in &self.inv_vectors {
            invv.kind.serialize(&mut payload);
            invv.hash.serialize(&mut payload);
        }
        Message {
            command: inv_ascii,
            payload: payload,
        }.serialize()
    }
    fn deserialize(payload: &mut Vec<u8>) -> Inv {
        let mut inv = Inv {
            inv_vectors: Vec::new(),
        };
        let vectors_len: u32 = Encodable::deserialize(payload);
        for _ in 0..vectors_len {
            inv.inv_vectors.push(InvVector {
                kind: Encodable::deserialize(payload),
                hash: Encodable::deserialize(payload),
            })
        }
        inv
    }
}

struct InvVector {
    // 1 block
    // 2 transaction
    kind: u16,
    hash: [u8; 32],
}

impl Message {
    fn serialize(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        // magic numbers
        [0xF9, 0xBE, 0xB4, 0xD9].serialize(&mut out);
        self.command.serialize(&mut out);
        (self.payload.len() as u32).serialize(&mut out);
        // checksum
        out.extend_from_slice(&sha_256_bytes(&sha_256_bytes(&self.payload))
            [..4]
            .to_vec());
        out.extend(&self.payload);
        out
    }

    fn deserialize(buf: &mut Vec<u8>) -> Message {
        // TODO confirm magic numbers
        let _magic_number: [u8; 4] = Encodable::deserialize(buf);
        let command: [u8; 12] = Encodable::deserialize(buf);
        let len: u32 = Encodable::deserialize(buf);
        let _checksum: [u8; 4] = Encodable::deserialize(buf);
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(&buf[..(len as usize)]);
        Message {
            command: command,
            payload: payload,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::{Addr, Message};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::collections::HashMap;
    use std::net;

    #[test]
    fn ascii_verify() {
        assert_eq!(
            String::from_utf8_lossy(&[
                0x70, 0x6f, 0x6e, 0x67, 0, 0, 0, 0, 0, 0, 0, 0
            ]),
            "pong\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}\u{0}".to_string()
        );
    }

    #[test]
    fn message_serialization() {
        let msg = Message {
            command: [0; 12],
            payload: Vec::new(),
        };
        assert_eq!(
            msg.serialize(),
            Message::deserialize(&mut msg.serialize()).serialize()
        );
    }

    #[test]
    fn addr_message_serialization() {
        let addr = Addr {
            ts: 0u64,
            address: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                8080,
            ),
        };
        let mut active_nodes: HashMap<net::SocketAddr, u64> = HashMap::new();
        active_nodes.insert(addr.address, 0);
        let mut msg = Addr::serialize(&active_nodes);
        assert_eq!(
            msg.serialize(),
            Message::deserialize(&mut msg.serialize()).serialize()
        );
        assert_eq!(
            Addr::deserialize(&mut msg.payload)[0].address,
            addr.address
        );
    }
}