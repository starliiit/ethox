use super::*;
use crate::managed::Slice;
use crate::nic::{external::External, Device};
use crate::layer::{eth, ip, arp};
use crate::wire::{EthernetAddress, Ipv4Address, IpCidr, PayloadMut, ethernet_frame, arp_packet, EthernetProtocol, ArpOperation};

const MAC_ADDR_HOST: EthernetAddress = EthernetAddress([0, 1, 2, 3, 4, 5]);
const IP_ADDR_HOST: Ipv4Address = Ipv4Address::new(127, 0, 0, 1);
const MAC_ADDR_OTHER: EthernetAddress = EthernetAddress([6, 5, 4, 3, 2, 1]);
const IP_ADDR_OTHER: Ipv4Address = Ipv4Address::new(127, 0, 0, 2);

struct SimpleSend;

#[test]
fn simple_arp() {
    let mut nic = External::new_send(Slice::One(vec![0; 1024]));

    let mut eth = [eth::Neighbor::default(); 1];
    let mut eth = eth::Endpoint::new(MAC_ADDR_HOST, {
        let mut eth_cache = eth::NeighborCache::new(&mut eth[..]);
        // No ARP cache entries needed.
        eth_cache
    });

    let mut ip = [ip::Route::unspecified(); 2];
    let mut ip = ip::Endpoint::new(IpCidr::new(IP_ADDR_HOST.into(), 24), {
        let ip_routes = ip::Routes::new(&mut ip[..]);
        // No routes necessary for local link.
        ip_routes
    });

    let mut arp = arp::Endpoint::new();

    let sent = nic.tx(1, eth.send(arp.send(&mut ip, SimpleSend { })));
    assert_eq!(sent, Ok(1));

    {
        // Retarget the packet to self.
        let buffer = nic.get_mut(0).unwrap();
        let eth = ethernet_frame::new_unchecked_mut(buffer);
        eth.set_dst_addr(MAC_ADDR_HOST);
        eth.set_src_addr(MAC_ADDR_OTHER);
    }

    // Set the buffer to be received.
    nic.receive_all();

    let recv = nic.rx(1,
                      eth.recv(arp.answer(&mut ip)));
    assert_eq!(recv, Ok(1));

    let buffer = nic.get_mut(0).unwrap();
    let eth = ethernet_frame::new_unchecked_mut(buffer);
    assert_eq!(eth.dst_addr(), MAC_ADDR_OTHER);
    assert_eq!(eth.src_addr(), MAC_ADDR_HOST);
    assert_eq!(eth.ethertype(), EthernetProtocol::Arp);

    let arp = arp_packet::new_unchecked_mut(eth.payload_mut_slice());
    assert_eq!(arp.operation(), ArpOperation::Reply);
    assert_eq!(arp.source_hardware_addr(), MAC_ADDR_HOST);
    assert_eq!(arp.source_protocol_addr(), IP_ADDR_HOST);
    assert_eq!(arp.target_hardware_addr(), MAC_ADDR_OTHER);
    assert_eq!(arp.target_protocol_addr(), IP_ADDR_OTHER);
}

impl<P: PayloadMut> arp::Send<P> for SimpleSend {
    fn send(&mut self, packet: RawPacket<P>) {
        let init = arp::Init::EthernetIpv4Request {
            source_hardware_addr: MAC_ADDR_OTHER,
            source_protocol_addr: IP_ADDR_OTHER.into(),
            target_hardware_addr: Default::default(),
            target_protocol_addr: IP_ADDR_HOST.into(),
        };
        let packet = packet.prepare(init)
            .expect("Can initialize to the host");
        packet
            .send()
            .expect("Can send the packet");
    }
}
