use crate::layer::{Error, Result, eth};
use crate::nic::{self, Info};
use crate::time::Instant;
use crate::wire::{ethernet, ip};
use crate::wire::{Checksum, Reframe, Payload, PayloadMut, PayloadResult, payload};

/// An incoming packet.
///
/// The contents were inspected and could be handled up to the ip layer.
pub struct In<'a, P: Payload> {
    /// A reference to the IP endpoint state.
    pub control: Controller<'a>,
    /// The valid packet inside the buffer.
    pub packet: IpPacket<'a, P>,
}

/// An outgoing packet as prepared by the ip layer.
///
/// While the layers below have been initialized, the payload of the packet has not. Fill it by
/// grabbing the mutable slice for example.
#[must_use = "You need to call `send` explicitely on an OutPacket, otherwise no packet is sent."]
pub struct Out<'a, P: Payload> {
    control: Controller<'a>,
    packet: IpPacket<'a, P>,
}

/// A buffer into which a packet can be placed.
pub struct Raw<'a, P: Payload> {
    /// A reference to the IP endpoint state.
    pub control: Controller<'a>,
    /// A mutable reference to the payload buffer.
    pub payload: &'a mut P,
}

/// A reference to the endpoint of layers below (phy + eth + ip).
///
/// This is not really useful on its own but should instead be used either within an [`InPacket`],
/// or a [`RawPacket`] or an [`OutPacket`]. Some of the methods offered there will access the
/// non-public members of this struct to fulfill their task.
///
/// [`InPacket`]: struct.InPacket.html
/// [`RawPacket`]: struct.RawPacket.html
/// [`OutPacket`]: struct.OutPacket.html
pub struct Controller<'a> {
    pub(crate) eth: eth::Controller<'a>,
    pub(crate) endpoint: &'a mut dyn Endpoint,
}

/// An IPv4 packet within an ethernet frame.
pub type V4Packet<'a, P> = ip::v4::Packet<ethernet::Frame<&'a mut P>>;
/// An IPv6 packet within an ethernet frame.
pub type V6Packet<'a, P> = ip::v6::Packet<ethernet::Frame<&'a mut P>>;

/// A valid IP packet buffer.
///
/// This provides a unified view on the payload and the source and destination addresses.
pub enum IpPacket<'a, P: Payload> {
    /// Containing an IPv4 packet.
    V4(V4Packet<'a, P>),
    /// Containing an IPv6 packet.
    V6(V6Packet<'a, P>),
}

/// Initializer for a packet.
#[derive(Copy, Clone, Debug)]
pub struct Init {
    /// The source selection method to use.
    pub source: Source,
    /// The destination address from which the next hop is derived.
    pub dst_addr: ip::Address,
    /// The wrapped protocol in the payload.
    pub protocol: ip::Protocol,
    /// The length to reserved for the payload.
    pub payload: usize,
}

/// A source selector specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The source address must match a subnet.
    Mask {
        /// The subnet mask which should contain the source address.
        subnet: ip::Subnet,
    },

    /// Some preselected address should be used.
    ///
    /// Required for established connections that are identified by an address tuple, such as in
    /// the case of TCP and UDP.
    Exact(ip::Address),
}

/// Source and destination chosen for a particular routing.
pub(crate) struct Route {
    pub(crate) next_hop: ip::Address,
    pub(crate) src_addr: ip::Address,
}

#[derive(Clone, Copy)]
struct EthRoute {
    src_mac: ethernet::Address,
    src_addr: ip::Address,
    next_mac: ethernet::Address,
}

/// The interface to the endpoint.
pub(crate) trait Endpoint{
    /// Get the ip to use on a link by providing the subnet in which it should be routed.
    fn local_ip(&self, subnet: ip::Subnet) -> Option<ip::Address>;
    /// Find a Route a destination at the current time.
    fn route(&self, dst_addr: ip::Address, time: Instant) -> Option<Route>;
    /// Resolve an address. If `look` is true, try to actively lookup it up later.
    fn resolve(&mut self, _: ip::Address, _: Instant, look: bool) -> Result<ethernet::Address>;
}

impl<'a> Controller<'a> {
    pub(crate) fn wrap(self,
        wrap: impl FnOnce(&'a mut dyn nic::Handle) -> &'a mut dyn nic::Handle,
    ) -> Self {
        let eth = self.eth.wrap(wrap);
        Controller { eth, endpoint: self.endpoint }
    }

    /// Get the hardware info for that packet.
    pub fn info(&self) -> &dyn Info {
        self.eth.info()
    }

    /// Proof to the compiler that we can shorten the lifetime arbitrarily.
    pub fn borrow_mut(&mut self) -> Controller {
        Controller {
            eth: self.eth.borrow_mut(),
            endpoint: self.endpoint,
        }
    }

    /// Get the local endpoint IP to use as source on some subnet.
    pub fn local_ip(&self, subnet: ip::Subnet) -> Option<ip::Address> {
        self.endpoint.local_ip(subnet)
    }

    /// Try to initialize the destination from an upper layer protocol address.
    ///
    /// Failure to satisfy the request is clearly signalled. Use the result to initialize the
    /// representation to a valid eth frame.
    pub fn resolve(&mut self, dst_addr: ip::Address)
        -> Result<ethernet::Address>
    {
        let time = self.info().timestamp();
        self.endpoint.resolve(dst_addr, time, true)
    }

    fn route_to(&mut self, dst_addr: ip::Address) -> Result<EthRoute> {
        let now = self.eth.info().timestamp();
        let Route { next_hop, src_addr } = self.endpoint
            .route(dst_addr, now)
            .ok_or(Error::Unreachable)?;
        let next_mac = self.resolve(next_hop)?;
        let src_mac = self.eth.src_addr();

        Ok(EthRoute {
            src_mac,
            src_addr,
            next_mac,
        })
    }
}

impl<'a, P: Payload> In<'a, P> {
    /// Deconstruct the packet into the reusable buffer.
    pub fn deinit(self) -> Raw<'a, P>
        where P: PayloadMut,
    {
        Raw {
            control: self.control,
            payload: self.packet.into_raw()
        }
    }
}

impl<'a, P: PayloadMut> In<'a, P> {
    /// Reinitialize the buffer with a packet generated by the library.
    // TODO: guarantee payload preserved?
    pub fn reinit(mut self, init: Init) -> Result<Out<'a, P>> {
        let route = self.control.route_to(init.dst_addr)?;
        let lower_init = init.init_eth(route, init.payload)?;

        let eth_packet = eth::InPacket {
            control: self.control.eth,
            frame: self.packet.into_inner(),
        };

        // TODO: optimize in case frame already contains the right IP packet.
        let packet = eth_packet.reinit(lower_init)?;
        let eth::InPacket { control, mut frame } = packet.into_incoming();
        let repr = init.initialize(route.src_addr, &mut frame)?;

        Ok(Out {
            control: Controller {
                eth: control,
                endpoint: self.control.endpoint,
            },
            packet: IpPacket::new_unchecked(frame, repr),
        })
    }
}

impl<'a, P: Payload> Out<'a, P> {
    /// Pretend the packet has been initialized by the ip layer.
    ///
    /// This is fine to call if a previous call to `into_incoming` was used to destructure the
    /// initialized packet and its contents have not changed. Some changes are fine as well and
    /// nothing will cause unsafety but panics or dropped packets are to be expected.
    pub fn new_unchecked(
        control: Controller<'a>,
        packet: IpPacket<'a, P>) -> Self
    {
        Out { control, packet, }
    }

    /// Unwrap the contained control handle and initialized ethernet frame.
    pub fn into_incoming(self) -> In<'a, P> {
        let Out { control, packet } = self;
        In { control, packet }
    }

    /// Retrieve the representation of the prepared packet.
    ///
    /// May be useful to check on the result of the ip layer logic before sending a packet.
    pub fn repr(&self) -> ip::Repr {
        self.packet.repr()
    }
}

impl<'a, P: PayloadMut> Out<'a, P> {
    /// Called last after having initialized the payload.
    ///
    /// This will also take care of filling the checksums as required.
    pub fn send(mut self) -> Result<()> {
        let capabilities = self.control.info().capabilities();
        match &mut self.packet {
            IpPacket::V4(ipv4) => {
                // Recalculate the checksum if necessary.
                ipv4.fill_checksum(capabilities.ipv4().tx_checksum());
            },
            _ => (),
        }
        let lower = eth::OutPacket::new_unchecked(
            self.control.eth,
            self.packet.into_inner());
        lower.send()
    }

    /// A mutable slice containing the payload of the contained protocol.
    ///
    /// This returns the IPv4 and IPv6 payload respectively. Note that the checksum is finalized
    /// only when `send` is called so you can mutate the buffer at will.
    ///
    /// TODO: A potential future extension might offer the ability precompute the checksum and to
    /// update the buffer and checksum in a single operation.
    pub fn payload_mut_slice(&mut self) -> &mut [u8] {
        self.packet.payload_mut().as_mut_slice()
    }
}

impl<'a, P: Payload + PayloadMut> Raw<'a, P> {
    pub fn control(&self) -> &Controller<'a> {
        &self.control
    }

    /// Initialize to a valid ip packet.
    pub fn prepare(mut self, init: Init) -> Result<Out<'a, P>> {
        let route = self.control.route_to(init.dst_addr)?;
        let lower_init = init.init_eth(route, init.payload)?;

        let lower = eth::RawPacket {
            control: self.control.eth,
            payload: self.payload,
        };

        let packet = lower.prepare(lower_init)?;
        let eth::InPacket { control, mut frame } = packet.into_incoming();
        let repr = init.initialize(route.src_addr, &mut frame)?;

        Ok(Out {
            control: Controller {
                eth: control,
                endpoint: self.control.endpoint,
            },
            packet: IpPacket::new_unchecked(frame, repr),
        })
    }
}

impl Init {
    fn initialize(&self, src_addr: ip::Address, payload: &mut impl PayloadMut) -> Result<ip::Repr> {
        let repr = self.ip_repr(src_addr)?;
        // Emit the packet but ignore the checksum for now. it is filled in later when calling
        // `OutPacket::send`.
        repr.emit(payload.payload_mut().as_mut_slice(), Checksum::Ignored);
        Ok(repr)
    }

    /// Resolve the ip representation without initializing the packet.
    fn ip_repr(&self, src_addr: ip::Address) -> Result<ip::Repr> {
        let repr = ip::Repr::Unspecified {
            src_addr,
            dst_addr: self.dst_addr,
            hop_limit: u8::max_value(),
            protocol: self.protocol,
            payload_len: self.payload,
        };
        repr.lower(&[]).ok_or(Error::Illegal)
    }

    fn init_eth(&self, route: EthRoute, payload: usize) -> Result<eth::Init> {
        enum Protocol { Ipv4, Ipv6 }

        let protocol = match self.dst_addr {
            ip::Address::Ipv4(_) => Protocol::Ipv4,
            ip::Address::Ipv6(_) => Protocol::Ipv6,
            _ => return Err(Error::Illegal),
        };

        let eth_init = eth::Init {
            src_addr: route.src_mac,
            dst_addr: route.next_mac,
            ethertype: match protocol {
                Protocol::Ipv4 => ethernet::EtherType::Ipv4,
                Protocol::Ipv6 => ethernet::EtherType::Ipv6,
            },
            // TODO: use the methods provided from `wire::*Repr`.
            payload: match protocol {
                Protocol::Ipv4 => payload + 20,
                // TODO: non-hardcode for extension headers.
                Protocol::Ipv6 => payload + 40,
            },
        };
        Ok(eth_init)
    }
}

impl<'a, P: Payload> IpPacket<'a, P> {
    /// Assemble an ip packet with already computed representation.
    ///
    /// # Panics
    /// This function panics if the representation is not specifically Ipv4 or Ipv6.
    pub fn new_unchecked(inner: ethernet::Frame<&'a mut P>, repr: ip::Repr) -> Self {
        match repr {
            ip::Repr::Ipv4(repr) => IpPacket::V4(ip::v4::Packet::new_unchecked(inner, repr)),
            ip::Repr::Ipv6(repr) => IpPacket::V6(ip::v6::Packet::new_unchecked(inner, repr)),
            _ => panic!("Unchecked must be from specific ip representation"),
        }
    }

    /// Retrieve the representation of the packet.
    pub fn repr(&self) -> ip::Repr {
        match self {
            IpPacket::V4(packet) => packet.repr().into(),
            IpPacket::V6(packet) => packet.repr().into(),
        }
    }

    /// Turn the packet into its ethernet layer respresentation.
    pub fn into_inner(self) -> ethernet::Frame<&'a mut P> {
        match self {
            IpPacket::V4(packet) => packet.into_inner(),
            IpPacket::V6(packet) => packet.into_inner(),
        }
    }

    /// Retrieve the payload of the packet.
    ///
    /// This is a utility wrapper around unwrapping the inner ethernet frame.
    pub fn into_raw(self) -> &'a mut P {
        self.into_inner().into_inner()
    }
}

impl<'a, P: Payload> Payload for IpPacket<'a, P> {
    fn payload(&self) -> &payload {
        match self {
            IpPacket::V4(packet) => packet.payload(),
            IpPacket::V6(packet) => packet.payload(),
        }
    }
} 

impl<'a, P: PayloadMut> PayloadMut for IpPacket<'a, P> {
    fn payload_mut(&mut self) -> &mut payload {
        match self {
            IpPacket::V4(packet) => packet.payload_mut(),
            IpPacket::V6(packet) => packet.payload_mut(),
        }
    }

    fn resize(&mut self, length: usize) -> PayloadResult<()> {
        match self {
            IpPacket::V4(packet) => packet.resize(length),
            IpPacket::V6(packet) => packet.resize(length),
        }
    }

    fn reframe(&mut self, frame: Reframe) -> PayloadResult<()> {
        match self {
            IpPacket::V4(packet) => packet.reframe(frame),
            IpPacket::V6(packet) => packet.reframe(frame),
        }
    }
} 

impl From<ip::Address> for Source {
    fn from(address: ip::Address) -> Self {
        Source::Exact(address)
    }
}

impl From<ip::Subnet> for Source {
    fn from(subnet: ip::Subnet) -> Self {
        Source::Mask { subnet, }
    }
}
