/// Defines the state machine for a single connection.
///
/// A `Connection` is a Mealy machine receiving `InPacket` from the network, returning `Signals` to
/// the rest of the TCP layer. In the other direction, the transmit portion of the stack
/// communicates the user buffers `AvailableBytes` and `ReceivedSegment` to affect the `Segment`
/// emitted in the transmission part.

use core::convert::TryFrom;
use core::ops::Range;
use crate::time::{Duration, Expiration, Instant};
use crate::wire::{ip::Address, tcp};

use super::endpoint::{
    Entry,
    EntryKey,
    FourTuple,
    Slot,
    SlotKey};

/// The state of a connection.
///
/// Includes current state machine state, the configuration state that is required to stay constant
/// during a connection, and the in- and out-buffers.
#[derive(Clone, Copy, Debug, Hash)]
pub struct Connection {
    /// The current state of the state machine.
    pub current: State,

    /// The previous state of the state machine.
    ///
    /// Required to correctly reset the state in closing the connection at RST. It is necessary to
    /// track *how* we ended up forming a (half-open) connection.
    pub previous: State,

    /// The flow control mechanism.
    ///
    /// Currently hard coded as TCP Reno but practically could also be an enum when we find a
    /// suitable common interface.
    pub flow_control: Flow,

    /// The indicated receive window (rcwd) of the other side.
    pub receive_window: u32,

    /// The SMSS is the size of the largest segment that the sender can transmit.
    ///
    /// This value can be based on the maximum transmission unit of the network, the path MTU
    /// discovery [RFC1191, RFC4821] algorithm, RMSS (see next item), or other factors.  The size
    /// does not include the TCP/IP headers and options.
    pub sender_maximum_segment_size: u16,

    /// The RMSS is the size of the largest segment the receiver is willing to accept.
    ///
    /// This is the value specified in the MSS option sent by the receiver during connection
    /// startup.  Or, if the MSS option is not used, it is 536 bytes [RFC1122].  The size does not
    /// include the TCP/IP headers and options.
    pub receiver_maximum_segment_size: u16,

    /// The received byte offset when the last ack was sent.
    ///
    /// We SHOULD wait at most 2*RMSS bytes before sending the next ack. There is also a time
    /// requirement, see `last_ack_time`.
    pub last_ack_receive_offset: tcp::SeqNumber,

    /// The time when the next ack must be sent.
    ///
    /// We MUST NOT wait more than 500ms before sending the ACK after receiving some new segment
    /// bytes. However, we CAN wait shorter, see `ack_timeout`.
    pub ack_timer: Expiration,

    /// Timeout before sending the next ACK after a new segment.
    ///
    /// For compliance with RFC1122 this MUST NOT be greater than 500ms but it could be smaller.
    pub ack_timeout: Duration,

    /// When to start retransmission and/or detect a loss.
    pub retransmission_timer: Instant,

    /// The duration of the retransmission timer.
    pub retransmission_timeout: Duration,

    /// Timeout of no packets in either direction after which restart is used.
    ///
    /// This will only occur if no data is to be transmitted in either direction as otherwise we
    /// would try sending or receive at least recovery packets. Well, the user could not have
    /// called us for a very long time but then this is also fine.
    pub restart_timeout: Duration,

    /// If we are permitted to use SACKs.
    ///
    /// This is true if the SYN packet allowed it in its options since we support it [WIP].
    pub selective_acknowledgements: bool,

    /// Counter of duplicated acks.
    pub duplicate_ack: u8,

    /// The sending state.
    ///
    /// In RFC793 this is referred to as `SND`.
    pub send: Send,

    /// The receiving state.
    ///
    /// In RFC793 this is referred to as `RCV`.
    pub recv: Receive,
}

/// The connection state relevant for outgoing segments.
#[derive(Clone, Copy, Debug, Hash)]
pub struct Send {
    /// The next not yet acknowledged sequence number.
    ///
    /// In RFC793 this is referred to as `SND.UNA`.
    pub unacked: tcp::SeqNumber,

    /// The next sequence number to use for transmission.
    ///
    /// In RFC793 this is referred to as `SND.NXT`.
    pub next: tcp::SeqNumber,

    /// The time of the last valid packet.
    pub last_time: Instant,

    /// Number of bytes available for sending in total.
    ///
    /// In contrast to `unacked` this is the number of bytes that have not yet been sent. The
    /// driver will update this number prior to sending or receiving packets so that an optimal
    /// answer packet can be determined.
    pub unsent: usize,

    /// The send window size indicated by the receiver.
    ///
    /// Must not send packet containing a sequence number beyond `unacked + window`. In RFC793 this
    /// is referred to as `SND.WND`.
    pub window: u16,

    /// The window scale parameter.
    ///
    /// Guaranteed to be at most 14 so that shifting the window in a `u32`/`i32` is always safe.
    pub window_scale: u8,

    /// The initial sequence number.
    ///
    /// This is read-only and only kept for potentially reading it for debugging later. It
    /// essentially provides a way of tracking the sent data. In RFC793 this is referred to as
    /// `ISS`.
    pub initial_seq: tcp::SeqNumber,
}

/// The connection state relevant for incoming segments.
#[derive(Clone, Copy, Debug, Hash)]
pub struct Receive {
    /// The next expected sequence number.
    ///
    /// In comparison the RFC validity checks are done with `acked` to implemented delayed ACKs but
    /// appear consistent to the outside. In RFC793 this is referred to as `RCV.NXT`.
    pub next: tcp::SeqNumber,

    /// The actually acknowledged sequence number.
    ///
    /// Implementing delayed ACKs (not sending acks for every packet) this tracks what we have
    /// publicly announced as our `NXT` sequence. Validity checks of incoming packet should be done
    /// relative to this value instead of `next`. In Linux, this is called `wup`.
    pub acked: tcp::SeqNumber,

    /// The time the last segment was sent.
    pub last_time: Instant,

    /// The receive window size indicated by us.
    ///
    /// Incoming packet containing a sequence number beyond `unacked + window`. In RFC793 this
    /// is referred to as `SND.WND`.
    pub window: u16,

    /// The window scale parameter.
    ///
    /// Guaranteed to be at most 14 so that shifting the window in a `u32`/`i32` is always safe.
    pub window_scale: u8,

    /// The initial receive sequence number.
    ///
    /// This is read-only and only kept for potentially reading it for debugging later. It
    /// essentially provides a way of tracking the sent data. In RFC793 this is referred to as
    /// `ISS`.
    pub initial_seq: tcp::SeqNumber,
}

/// State enum of the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    /// Marker state fo an unintended/uninitialized connection state.
    Closed,

    /// A listening connection.
    ///
    /// Akin to an open server socket. Can either be turned into SynSent or SynReceived depending
    /// on whether we receive a SYN or decide to open a connection.
    Listen,

    /// An open connection request.
    SynSent,

    /// Connection request we intend to answer, waiting on ack.
    SynReceived,

    /// An open connection.
    Established,

    /// Closed our side of the connection.
    ///
    /// This is split into two states (FinWait1 and FinWait2) in the RFC where we track whether our
    /// own FIN has been ack'ed. This is of importance for answering CLOSE calls but can be
    /// supplemented in the Io implementation. Transition to the TimeWait state works the same.
    FinWait,

    /// Closed both sides but we don't know the other knows.
    Closing,

    /// Both sides recognized connection as closed.
    TimeWait,

    /// Other side closed its connection.
    CloseWait,

    /// Connection closed after other side closed its already.
    LastAck,
}

/// Models TCP Reno flow control and congestion avoidance.
#[derive(Clone, Copy, Debug, Hash)]
pub struct Flow {
    /// Decider between slow-start and congestion.
    ///
    /// Set to MAX initially, then updated on occurrence of congestion.
    pub ssthresh: u32,

    /// The window dictated by congestion.
    pub congestion_window: u32,

    /// Sender side end flag to fast recover.
    ///
    /// When in fast recover, declares the sent sequent number that must be acknowledged to end
    /// fast recover. Initially set to the initial sequence number (ISS).
    pub recover: tcp::SeqNumber,
}

/// Output signals of the model.
///
/// Private representation since they also influence handling of the state itself.
#[derive(Clone, Copy, Default, Debug)]
#[must_use = "Doesn't do anything on its own, make sure any answer is actually sent."]
pub struct Signals {
    /// If the state should be deleted.
    pub delete: bool,

    /// The user should be notified of this reset connection.
    pub reset: bool,

    /// There is valid data in the packet to receive.
    pub receive: Option<ReceivedSegment>,

    /// Whether the Operator could send data.
    pub may_send: bool,

    /// Need to send some tcp answer.
    ///
    /// Since TCP must assume every packet to be potentially lost it is likely technically fine
    /// *not* to actually send the packet. In particular you could probably advance the internal
    /// state without acquiring packets to send out. This, however, sounds like a very bad idea.
    pub answer: Option<tcp::Repr>,
}

/// A descriptor of the transmission buffer.
///
///
#[derive(Clone, Copy, Debug)]
pub struct AvailableBytes {
    /// Set when no more data will come.
    pub fin: bool,

    /// The total number of bytes buffered for retransmission and newly available.
    pub total: usize,
}

/// A descriptor of an accepted incoming segment.
///
/// This acknowledges a segment that has been accepted by the receive/reassembly buffer, advancing
/// the outgoing ACKs and other related state. See [`Connection::set_recv_ack`] for details.
///
/// [`Connection::set_recv_ack`]: struct.Connection.set_recv_ack
#[derive(Clone, Copy, Debug)]
#[must_use = "Pass this to `Connection::set_recv_ack` after read the segment."]
pub struct ReceivedSegment {
    /// If the segment has a syn.
    ///
    /// SYN occupies one sequence space before the actual data.
    pub syn: bool,

    /// If the segment has a fin.
    ///
    /// FIN occupies one sequence space after the data.
    pub fin: bool,

    /// The length of the actual data.
    pub data_len: usize,

    /// The sequence number at the start of this packet.
    pub begin: tcp::SeqNumber,

    /// Timestamp for acking this segment.
    pub timestamp: Instant,
}

/// An ingoing communication.
#[derive(Debug)]
pub struct InPacket {
    /// Metadata of the tcp layer packet.
    pub segment: tcp::Repr,

    /// The sender address.
    pub from: Address,

    /// The arrival time of the packet at the nic.
    pub time: Instant,
}

/// An outgoing segment.
#[derive(Clone, Debug)]
pub struct Segment {
    /// Representation for the packet.
    pub repr: tcp::Repr,

    /// Range of the data that should be included, as indexed within the (re-)transmit buffer.
    pub range: Range<usize>,
}

/// Output signals of the model.
///
/// Private representation since they also influence handling of the state itself.
#[derive(Clone, Default, Debug)]
#[must_use = "Doesn't do anything on its own, make sure any answer is actually sent."]
pub struct OutSignals {
    pub delete: bool,

    /// A packet was selected to be generated.
    ///
    /// Some packets (ACKs or during connection closing) are only generated after the data of an
    /// incoming segment has been read.
    pub segment: Option<Segment>,
}

/// An internal, lifetime erased trait for controlling connections of an `Endpoint`.
///
/// This decouples the required interface for a packet from the implementation details of
/// `Endpoint` which are the user-facing interaction points. Partially necessary since we don't
/// want to expose the endpoint's lifetime to the packet handler but also to establish a somewhat
/// cleaner boundary.
pub trait Endpoint {
    fn get(&self, index: SlotKey) -> Option<&Slot>;

    fn get_mut(&mut self, index: SlotKey) -> Option<&mut Slot>;

    fn entry(&mut self, index: SlotKey) -> Option<Entry>;

    fn remove(&mut self, index: SlotKey);

    fn find_tuple(&mut self, tuple: FourTuple) -> Option<Entry>;

    fn source_port(&mut self, addr: Address) -> Option<u16>;

    fn listen(&mut self, ip: Address, port: u16) -> Option<SlotKey>;

    fn open(&mut self, tuple: FourTuple) -> Option<SlotKey>;

    fn initial_seq_num(&mut self, id: FourTuple, time: Instant) -> tcp::SeqNumber;
}

/// The interface to a single active connection on an endpoint.
pub(crate) struct Operator<'a> {
    pub(crate) endpoint: &'a mut dyn Endpoint,
    pub(crate) connection_key: SlotKey,
}

/// Internal return determining how a received ack is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AckUpdate {
    TooLow,
    Duplicate,
    Updated {
        new_bytes: u32
    },
    Unsent,
}

/// Tcp repr without the connection meta data.
#[derive(Clone, Copy, Debug)]
struct InnerRepr {
    flags:        tcp::Flags,
    seq_number:   tcp::SeqNumber,
    ack_number:   Option<tcp::SeqNumber>,
    window_len:   u16,
    window_scale: Option<u8>,
    max_seg_size: Option<u16>,
    sack_permitted: bool,
    sack_ranges:  [Option<(u32, u32)>; 3],
    payload_len:  u16,
}

impl Connection {
    /// Construct a closed connection with zeroed state.
    pub fn zeroed() -> Self {
        Connection {
            current: State::Closed,
            previous: State::Closed,
            flow_control: Flow {
                ssthresh: 0,
                congestion_window: 0,
                recover: tcp::SeqNumber::default(),
            },
            receive_window: 0,
            sender_maximum_segment_size: 0,
            receiver_maximum_segment_size: 0,
            last_ack_receive_offset: tcp::SeqNumber::default(),
            ack_timer: Expiration::Never,
            ack_timeout: Duration::from_millis(0),
            retransmission_timer: Instant::from_millis(0),
            retransmission_timeout: Duration::from_millis(0),
            restart_timeout: Duration::from_millis(0),
            selective_acknowledgements: false,
            duplicate_ack: 0,
            send: Send {
                unacked: tcp::SeqNumber::default(),
                next: tcp::SeqNumber::default(),
                last_time: Instant::from_millis(0),
                unsent: 0,
                window: 0,
                window_scale: 0,
                initial_seq: tcp::SeqNumber::default(),
            },
            recv: Receive {
                next: tcp::SeqNumber::default(),
                acked: tcp::SeqNumber::default(),
                last_time: Instant::from_millis(0),
                window: 0,
                window_scale: 0,
                initial_seq: tcp::SeqNumber::default(),
            },
        }
    }

    /// Handle an arriving packet.
    pub fn arrives(&mut self, incoming: &InPacket, entry: EntryKey) -> Signals {
        match self.current {
            State::Closed => self.arrives_closed(incoming),
            State::Listen => self.arrives_listen(incoming, entry),
            State::SynSent => self.arrives_syn_sent(incoming, entry),
            State::Established | State::FinWait => self.arrives_established(incoming, entry),
            _ => unimplemented!(),
        }
    }

    /// Realize the effect of opening SYN packet.
    pub fn open(&mut self, time: Instant, entry: EntryKey)
        -> Result<(), crate::layer::Error>
    {
        match self.current {
            State::Closed | State::Listen => (),
            _ => return Err(crate::layer::Error::Illegal),
        }

        self.change_state(State::SynSent);
        self.send.initial_seq = entry.initial_seq_num(time);
        self.send.unacked = self.send.initial_seq;
        self.send.next = self.send.initial_seq + 1;
        // Schedule 'immediate' transmission.
        self.retransmission_timer = time;

        Ok(())
    }

    /// Answers packets on closed sockets with resets.
    ///
    /// Except when an RST flag is already set on the received packet. Probably the easiest packet
    /// flow.
    fn arrives_closed(&mut self, incoming: &InPacket) -> Signals {
        let segment = &incoming.segment;
        let mut signals = Signals::default();
        if segment.flags.rst() {
            // Avoid answering with RST when packet has RST set.
            // TODO: debug counters or tracing
            return signals;
        }

        if let Some(ack_number) = segment.ack_number {
            signals.answer = Some(InnerRepr {
                flags: tcp::Flags::RST,
                seq_number: ack_number,
                ack_number: None,
                window_len: 0,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None; 3],
                payload_len: 0,
            }.send_back(segment));
        } else {
            signals.answer = Some(InnerRepr {
                flags: tcp::Flags::RST,
                seq_number: tcp::SeqNumber(0),
                ack_number: Some(segment.seq_number + segment.sequence_len()),
                window_len: 0,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None; 3],
                payload_len: 0,
            }.send_back(segment));
        }

        return signals;
    }

    /// Handle an incoming packet in Listen state.
    fn arrives_listen(&mut self, incoming: &InPacket, mut entry: EntryKey)
        -> Signals
    {
        // TODO: SYN cookies. Ideally, we could extend the original mechanism to support timestamp,
        // sack, and window scale as well. Note that ts and sack require only a single flag bit in
        // the cookie; the state for timestamp can be restored from the ts-option in the Ack answer
        // to our Syn+Ack and we require only a flag to check if we had received a ts-option in the
        // Syn initially; while sack also only requires a flag to indicate its negotiation state.
        //
        // The harder part seems to be that syn cookies require a new operation within Signals.

        let InPacket { segment, from, time, } = incoming;
        let mut signals = Signals::default();

        if segment.flags.rst() {
            return signals;
        }

        if let Some(ack_number) = segment.ack_number { // What are you acking? A previous connection.
            signals.answer = Some(InnerRepr {
                flags: tcp::Flags::RST,
                seq_number: ack_number,
                ack_number: None,
                window_len: 0,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None; 3],
                payload_len: 0,
            }.send_back(segment));
            return signals;
        }

        if !segment.flags.syn() {
            // Doesn't have any useful flags. Why was this even sent?
            return signals;
        }

        let current_four = entry.four_tuple();
        let new_four = FourTuple {
            remote: *from,
            .. current_four
        };
        entry.set_four_tuple(new_four);
        self.recv.next = segment.seq_number + 1;
        self.recv.initial_seq = segment.seq_number;

        let isn = entry.initial_seq_num(*time);
        self.send.next = isn + 1;
        self.send.unacked = isn;
        self.send.initial_seq = isn;

        signals.answer = Some(InnerRepr {
            flags: tcp::Flags::RST,
            seq_number: isn,
            ack_number: Some(self.ack_all()),
            window_len: self.recv.window,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 3],
            payload_len: 0,
        }.send_to(new_four));

        signals
    }

    fn arrives_syn_sent(&mut self, incoming: &InPacket, entry: EntryKey)
        -> Signals
    {
        let InPacket { segment, from: _, time, } = incoming;

        if let Some(ack) = segment.ack_number {
            if ack <= self.send.initial_seq || ack > self.send.next {
                if segment.flags.rst() { // Discard the segment
                    return Signals::default();
                }

                // Packet out of window. Send a RST with fitting sequence number.
                let mut signals = Signals::default();
                signals.answer = Some(InnerRepr {
                    flags: tcp::Flags::RST,
                    seq_number: ack,
                    ack_number: Some(segment.seq_number),
                    window_len: 0,
                    window_scale: None,
                    max_seg_size: None,
                    sack_permitted: false,
                    sack_ranges: [None; 3],
                    payload_len: 0,
                }.send_back(segment));
                return signals;
            }
        }

        if segment.flags.rst() {
            // Can only reset the connection if you ack the SYN.
            if segment.ack_number.is_none() {
                return Signals::default();
            }

            return self.remote_reset_connection();
        }

        if !segment.flags.syn() {
            // No control flags at all.
            return Signals::default();
        }

        self.recv.initial_seq = segment.seq_number;
        self.recv.next = segment.seq_number + 1;
        self.send.window = segment.window_len;
        self.send.window_scale = segment.window_scale.unwrap_or(0);

        // TODO: better mss
        self.sender_maximum_segment_size = segment.max_seg_size
            .unwrap_or(536)
            .max(536);
        self.receiver_maximum_segment_size = self.sender_maximum_segment_size;

        if let Some(ack) = segment.ack_number {
            self.send.unacked = ack;
        }

        // The SYN didn't actually ack our SYN. So change to SYN-RECEIVED.
        if self.send.unacked == self.send.initial_seq {
            self.change_state(State::SynReceived);

            let mut signals = Signals::default();
            signals.answer = Some(self.send_open(true, entry.four_tuple()));
            return signals;
        }

        self.change_state(State::Established);
        // The rfc would immediately ack etc. We may want to send data and that requires the
        // cooperation of io. Defer but mark as ack required immediately.
        self.ack_timer = Expiration::When(*time);
        return Signals::default();
    }

    fn arrives_established(&mut self, incoming: &InPacket, entry: EntryKey) -> Signals {
        // TODO: time for RTT estimation, ...
        let InPacket { segment, from: _, time, } = incoming;

        let acceptable = self.ingress_acceptable(segment);

        if !acceptable {
            if segment.flags.rst() {
                return self.remote_reset_connection();
            }

            // TODO: find out why this triggers in a nice tcp connection (python -m http.server)
            return self.signal_ack_all(entry.four_tuple());
        }

        if segment.flags.syn() {
            debug_assert!(self.recv.in_window(segment.seq_number));

            // This is not acceptable, reset the connection.
            return self.signal_reset_connection(segment, entry);
        }

        let ack = match segment.ack_number {
            // Not good, but not bad either.
            None => return Signals::default(),
            Some(ack) => ack,
        };

        match self.send.incoming_ack(ack) {
            AckUpdate::Unsent => {
                // That acked something we hadn't sent yet. A madlad at the other end.
                // Ignore the packet but we ack back the previous state.
                return self.signal_ack_all(entry.four_tuple());
            },
            AckUpdate::Duplicate => {
                self.duplicate_ack = self.duplicate_ack.saturating_add(1);
                /*
                self.flow_control.ssthresh = unimplemented!();
                self.flow_control.congestion_window = unimplemented!();
                */
            },
            // This is a reordered packet, potentially an attack. Do nothing.
            AckUpdate::TooLow => (),
            AckUpdate::Updated { new_bytes } => {
                // No longer in fast retransmit.
                if self.duplicate_ack > 0 {
                    self.flow_control.congestion_window = self.flow_control.ssthresh;
                    self.duplicate_ack = 0;
                }
                self.send.window = segment.window_len;
                self.window_update(segment, new_bytes);
            },
        }

        // URG lol

        let segment_ack = ReceivedSegment {
            syn: segment.flags.syn(),
            fin: segment.flags.fin(),
            data_len: usize::from(segment.payload_len),
            begin: segment.seq_number,
            timestamp: *time,
        };

        if segment_ack.data_len == 0 {
            self.set_recv_ack(segment_ack);
            return Signals::default();
        }

        // Actually accept the segment data. Note that we do not control the receive buffer
        // ourselves but rather only know the precise buffer lengths at this point. Also, the
        // window we indicated to the remote may not reflect exactly what we can actually accept.
        // Furthermore, we a) want to piggy-back data on the ACK to reduce the number of packet
        // sent and b) may want to delay ACKs as given by data in flight and RTT considerations
        // such as RFC1122. Thus, we merely signal the presence of available data to the operator
        // above.
        let mut signals = Signals::default();
        signals.receive = Some(segment_ack);
        signals
    }

    /// Determine if a packet should be deemed acceptable on an open connection.
    ///
    /// See: https://tools.ietf.org/html/rfc793#page-40
    fn ingress_acceptable(&self, repr: &tcp::Repr) -> bool {
        match (repr.payload_len, self.recv.window) {
            (0, 0) => repr.seq_number == self.recv.next,
            (0, _) => self.recv.in_window(repr.seq_number),
            (_, 0) => false,
            (_, _) => self.recv.in_window(repr.seq_number)
                || self.recv.in_window(repr.seq_number + repr.payload_len.into() - 1),
        }
    }

    /// Close from an incoming reset.
    ///
    /// This shared logic is used by some states on receiving a packet with RST set.
    fn remote_reset_connection(&mut self) -> Signals {
        self.change_state(State::Closed);

        let mut signals = Signals::default();
        signals.reset = true;
        signals.delete = true;
        return signals;
    }

    /// Close due to invalid incoming packet.
    ///
    /// As opposed to `remote_reset_connection` this one is proactive and we send the RST.
    fn signal_reset_connection(&mut self, _segment: &tcp::Repr, entry: EntryKey) -> Signals {
        self.change_state(State::Closed);

        let mut signals = Signals::default();
        signals.reset = true;
        signals.delete = true;
        signals.answer = Some(InnerRepr {
            flags: tcp::Flags::RST,
            seq_number: self.send.next,
            ack_number: Some(self.ack_all()),
            window_len: 0,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 3],
            payload_len: 0,
        }.send_to(entry.four_tuple()));
        signals
    }

    /// Explicitly send an ack for all data, now.
    fn signal_ack_all(&mut self, remote: FourTuple) -> Signals {
        let mut signals = Signals::default();
        signals.answer = Some(self.repr_ack_all(remote));
        return signals;
    }

    /// Construct a segment acking all data but nothing else.
    fn segment_ack_all(&mut self, remote: FourTuple) -> Segment {
        Segment {
            repr: self.repr_ack_all(remote),
            range: 0..0,
        }
    }

    fn repr_ack_all(&mut self, remote: FourTuple) -> tcp::Repr {
        InnerRepr {
            flags: tcp::Flags::default(),
            seq_number: self.send.next,
            ack_number: Some(self.ack_all()),
            window_len: self.recv.window,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 3],
            payload_len: 0,
        }.send_to(remote)
    }

    /// Send a SYN.
    ///
    /// If `ack` is true then it also acknowledges received segments (i.e. this is a passive open).
    fn send_open(&mut self, ack: bool, to: FourTuple) -> tcp::Repr {
        let ack_number = if ack { Some(self.ack_all()) } else { None };
        InnerRepr {
            flags: tcp::Flags::SYN,
            seq_number: self.send.initial_seq,
            ack_number,
            window_len: 0,
            window_scale: Some(self.send.window_scale),
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 3],
            payload_len: 0,
        }.send_to(to)
    }

    /// Choose a next data segment to send.
    ///
    /// May choose to send an empty range for cases where there is no data to send but a delayed
    /// ACK is expected.
    pub fn next_send_segment(&mut self, mut available: AvailableBytes, time: Instant, entry: EntryKey)
        -> OutSignals
    {
        match self.current {
            State::Established | State::CloseWait => {
                self.select_send_segment(available, time, entry)
                    .map(OutSignals::segment)
                    .unwrap_or_else(OutSignals::none)
            },
            // When we have already sent our FIN, never send *new* data.
            State::FinWait | State::Closing | State::LastAck => {
                available.total = available.total.min(self.send.next - self.send.unacked);
                // FIXME: ensure fin bit is set for retransmissions of last segment.
                self.select_send_segment(available, time, entry)
                    .map(OutSignals::segment)
                    .unwrap_or_else(OutSignals::none)
            },
            State::Closed => {
                self.ensure_closed_ack(entry.four_tuple())
                    .map(OutSignals::segment)
                    .unwrap_or_else(OutSignals::none)
            },
            State::TimeWait => self.ensure_time_wait(time, entry),
            State::SynSent | State::SynReceived => {
                self.select_syn_retransmit(time, entry)
                    .map(OutSignals::segment)
                    .unwrap_or_else(OutSignals::none)
            },
            State::Listen => OutSignals::none(),
        }
    }

    fn select_send_segment(&mut self, available: AvailableBytes, time: Instant, entry: EntryKey)
        -> Option<Segment>
    {
        // Convert the input to `u32`, our window can never be that large anyways.
        let byte_window = u32::try_from(available.total)
            .ok().unwrap_or_else(u32::max_value);
        // Connection restarted after idle time.
        let last_time = self.recv.last_time.max(self.send.last_time);
        if time > last_time + self.restart_timeout {
            self.flow_control.congestion_window = self.restart_window();
        }

        if self.duplicate_ack >= 2 {
            // Fast retransmit?
            //
            // this would be a return path but just don't do anything atm.
            return self.fast_retransmit(available, time, entry);
        }

        if self.retransmission_timer < time {
            // Choose segments to retransmit, in contrast to `fast_retransmit` this may influence
            // multiple next packets.
            return self.timeout_retransmit(available, time, entry);
        }

        // That's funny. Even if we have sent a FIN, the other side could decrease their window
        // size to the point where we could not send the sequence number of the FIN again.
        let window = self.send.window();
            // TODO: congestion flow control
            // .min(self.flow_control.congestion_window);
        let sent = self.send.in_flight();
        let max_sent = window.min(byte_window);

        if sent < max_sent {
            // Send one new segment of new data.
            let end = sent.saturating_add(self.sender_maximum_segment_size.into()).min(max_sent);
            // UNWRAP: Available was larger than `end` so these will not fail (even on 16-bit
            // platforms where the buffer may be smaller than the `u32` window). Math:
            // `sent_u32 <= end_u32 <= available_u32 <= available_usize`
            let sent = usize::try_from(sent).unwrap();
            let end = usize::try_from(end).unwrap();
            let range = sent..end;
            assert!(range.len() > 0);

            let is_fin = available.fin && end as usize == available.total;

            if is_fin {
                match self.current {
                    State::Established => self.change_state(State::FinWait),
                    State::CloseWait => self.change_state(State::LastAck),
                    _ => (),
                }
            }

            let mut repr = self.repr_ack_all(entry.four_tuple());

            repr.payload_len = range.len() as u16;
            if is_fin {
                repr.flags = tcp::Flags::FIN;
            }

            self.send.next = self.send.next + range.len() + usize::from(is_fin);

            return Some(Segment {
                repr,
                range,
            });
        }

        // There is nothing to send but we may need to ack anyways.
        if self.should_ack() || Expiration::When(time) >= self.ack_timer {
            self.rearm_ack_timer(time);
            return Some(self.segment_ack_all(entry.four_tuple()));
        }

        None
    }

    fn select_syn_retransmit(&mut self, time: Instant, entry: EntryKey)
        -> Option<Segment>
    {
        if self.retransmission_timer > time {
            return None;
        }

        let ack = match self.current {
            State::SynReceived => true,
            State::SynSent => false,
            _ => unreachable!(),
        };

        self.rearm_retransmission_timer(time);
        Some(Segment {
            repr: self.send_open(ack, entry.four_tuple()),
            range: 0..0,
        })
    }

    fn fast_retransmit(&mut self, available: AvailableBytes, _: Instant, entry: EntryKey)
        -> Option<Segment>
    {
        // TODO: flow control, adjust window
        self.segment_retransmit(available, entry.four_tuple())
    }

    fn timeout_retransmit(&mut self, available: AvailableBytes, time: Instant, entry: EntryKey)
        -> Option<Segment>
    {
        self.rearm_retransmission_timer(time);
        self.segment_retransmit(available, entry.four_tuple())
    }

    fn segment_retransmit(&mut self, available: AvailableBytes, tuple: FourTuple) -> Option<Segment> {
        // See: https://tools.ietf.org/html/rfc5681#section-3.2
        // Retransmit the first unacknowledged segment. We can however also retransmit as much
        // bytes as we'd like starting at the first unacked segment. This is more efficient if that
        // was for some reason shorter than the mss.
        let in_flight = self.send.in_flight();

        let byte_window = u32::try_from(available.total)
            .ok().unwrap_or_else(u32::max_value);

        // That was a third duplicate ack but there is no data actually missing.
        if in_flight == 0 {
            return None;
        }

        let to_send = self.send.window()
            .min(u32::from(self.sender_maximum_segment_size))
            .min(byte_window);

        if to_send == 0 {
            return None;
        }

        let range = 0..usize::try_from(to_send).unwrap();
        let is_fin = available.fin && range.end == available.total;

        let mut repr = self.repr_ack_all(tuple);
        repr.flags.set_fin(is_fin);
        repr.seq_number = self.send.unacked;
        repr.payload_len = to_send as u16;

        Some(Segment {
            repr,
            range,
        })
    }

    fn ensure_closed_ack(&mut self, tuple: FourTuple) -> Option<Segment> {
        if self.recv.acked == self.recv.next {
            return None;
        }

        Some(self.segment_ack_all(tuple))
    }

    fn ensure_time_wait(&mut self, time: Instant, entry: EntryKey) -> OutSignals {
        match self.ensure_closed_ack(entry.four_tuple()) {
            Some(segment) => OutSignals {
                segment: Some(segment),
                delete: false,
            },
            None => OutSignals {
                delete: time >= self.retransmission_timer,
                segment: None,
            },
        }
    }

    fn window_update(&mut self, _segment: &tcp::Repr, new_bytes: u32) {
        let flow = &mut self.flow_control;
        if self.duplicate_ack > 0 {
            flow.congestion_window = flow.ssthresh;
        } else if flow.congestion_window <= flow.ssthresh {
            flow.congestion_window = flow.congestion_window.saturating_mul(2);
        } else {
            // https://tools.ietf.org/html/rfc5681, avoid cwnd flooding from ack splitting.
            let update = u32::from(self.sender_maximum_segment_size).min(new_bytes);
            flow.congestion_window = flow.congestion_window.saturating_add(update);
        }
    }

    /// Acknowledge that a received segment has reached the reader.
    ///
    /// This method trusts the content of the `ReceivedSegment`. In particular, its SYN/FIN bits,
    /// time stamp and length information should be of the last received packet. The best course of
    /// action is to only pass in exactly the value previously returned in the signals of a call to
    /// [`arrives`].
    ///
    /// Passing wrong information will not lead to memory safety concerns directly but you can no
    /// longer rely on the accuracy of subsequent connection state. The remote may also get
    /// incorrect ACKs, and connection resets might occur.
    ///
    /// [`arrives`]: #method.arrives
    pub fn set_recv_ack(&mut self, meta: ReceivedSegment) {
        let end = meta.sequence_end();
        let acked_all = self.send.next == self.send.unacked;

        match (self.current, meta.fin, acked_all) {
            (State::Established, true, _) | (State::SynReceived, true, _) => {
                self.change_state(State::CloseWait);
            },
            (State::FinWait, true, true) | (State::Closing, _, true) => {
                self.change_state(State::TimeWait);
                // We could have a segment lifetime estimation here, but use the retransmission
                // timeout instead. Works as well, I guess.
                self.retransmission_timer = meta.timestamp + 2*self.retransmission_timeout;
            },
            (State::FinWait, true, false) => {
                self.change_state(State::Closing);
            },
            _ => (),
        }

        self.recv.next = end;
        let new_timer = Expiration::When(meta.timestamp + self.ack_timeout);
        self.ack_timer = self.ack_timer.min(new_timer);
    }

    /// Get the sequence number of the last byte acknowledged by the other side.
    ///
    /// Always points into the byte sequence space by offsetting a missing SYN in case none has
    /// been received yet.
    pub fn get_send_ack(&self) -> tcp::SeqNumber {
        match self.current {
            // If our SYN has not been acked, advance beyond the SYN.
            State::SynSent => self.send.unacked + 1,
            // Don't include our FIN even if it has already been acked.
            State::FinWait | State::Closing | State::TimeWait | State::LastAck
                if self.send.unacked == self.send.next
                    => self.send.unacked - 1,
            _ => self.send.unacked,
        }
    }

    /// Indicate sending an ack for all arrived packets.
    ///
    /// When delaying acks for better throughput we split the recv ack counter into two: One for
    /// the apparent state of actually sent acknowledgments and one for the acks we have queued.
    /// Sending a packet with the current received state catches the former up to the latter
    /// counter.
    fn ack_all(&mut self) -> tcp::SeqNumber {
        self.recv.acked = self.recv.next;
        self.ack_timer = Expiration::Never;
        self.recv.next
    }

    /// Determine whether to send an ACK.
    ///
    /// This is currently always true when there is any sequence space to ack but that may change
    /// for delayed acks.
    fn should_ack(&self) -> bool {
        self.recv.acked < self.recv.next
    }

    fn rearm_ack_timer(&mut self, time: Instant) {
        self.ack_timer = match self.ack_timer {
            Expiration::When(_) => Expiration::When(time + self.ack_timeout),
            Expiration::Never => Expiration::Never,
        }
    }

    fn rearm_retransmission_timer(&mut self, time: Instant) {
        self.retransmission_timer = time + self.retransmission_timeout;
    }

    pub(crate) fn change_state(&mut self, new: State) {
        self.previous = self.current;
        self.current = new;
    }

    /// RFC5681 restart window.
    fn restart_window(&self) -> u32 {
        self.flow_control.congestion_window.min(self.send.window.into())
    }
}

impl Receive {
    fn in_window(&self, seq: tcp::SeqNumber) -> bool {
        self.next.contains_in_window(seq, self.window.into())
    }

    /// Setup the window based on an incoming (unscaled) window field.
    pub fn update_window(&mut self, window: usize) {
        let max = u32::from(u16::max_value()) << self.window_scale;
        let capped = u32::try_from(window)
            .unwrap_or_else(|_| u32::max_value())
            .min(max);
        let scaled_down = (capped >> self.window_scale)
            + u32::from(capped % (1 << self.window_scale) != 0);
        self.window = u16::try_from(scaled_down).unwrap();
    }
}

impl Send {
    fn incoming_ack(&mut self, seq: tcp::SeqNumber) -> AckUpdate {
        if seq < self.unacked {
            AckUpdate::TooLow
        } else if seq == self.unacked {
            AckUpdate::Duplicate
        } else if seq <= self.next {
            // FIXME: this calculation could be safe without `as` coercion.
            let new_bytes = (seq - self.unacked) as u32;
            self.unacked = seq;
            AckUpdate::Updated { new_bytes }
        } else {
            AckUpdate::Unsent
        }
    }

    /// Get the actual window (combination of indicated window and scale).
    fn window(&self) -> u32 {
        u32::from(self.window) << self.window_scale
    }

    /// Get the segments in flight.
    fn in_flight(&self) -> u32 {
        assert!(self.unacked <= self.next);
        (self.next - self.unacked) as u32
    }
}

impl ReceivedSegment {
    /// Compute the total length in sequence space, including SYN or FIN.
    pub fn sequence_len(&self) -> usize {
        self.data_len
            + usize::from(self.syn)
            + usize::from(self.fin)
    }

    /// Only ack part of the segment until some sequence point.
    ///
    /// Takes care of removing the FIN flag if the acked part does not cover every data byte until
    /// that point.
    pub fn acked_until(&self, ack: tcp::SeqNumber) -> Self {
        ReceivedSegment {
            syn: self.syn,
            fin: self.fin && ack + 1 >= self.sequence_end(),
            begin: self.begin,
            data_len: self.data_len,
            timestamp: self.timestamp,
        }
    }

    /// Returns the sequence number corresponding to the first data byte in this segment.
    pub fn data_begin(&self) -> tcp::SeqNumber {
        self.begin + usize::from(self.syn)
    }

    /// Returns the sequence number corresponding to the last data byte in this segment.
    pub fn data_end(&self) -> tcp::SeqNumber {
        self.begin + usize::from(self.syn) + self.data_len
    }

    /// Check if the given sequence number if within the window of this segment.
    pub fn contains_in_window(&self, seq: tcp::SeqNumber) -> bool {
        self.begin.contains_in_window(seq, self.sequence_len())
    }

    /// Returns the past-the-end sequence number with which to ACK the segment.
    pub fn sequence_end(&self) -> tcp::SeqNumber {
        self.begin + self.sequence_len()
    }
}

impl OutSignals {
    /// No segment and keep the tcb.
    pub fn none() -> Self {
        OutSignals::default()
    }

    /// Send a segment but do not delete.
    pub fn segment(segment: Segment) -> Self {
        OutSignals {
            segment: Some(segment),
            delete: false,
        }
    }
}

impl Operator<'_> {
    pub(crate) fn key(&self) -> SlotKey {
        self.connection_key
    }

    pub(crate) fn four_tuple(&self) -> FourTuple {
        self.slot().four_tuple()
    }

    pub(crate) fn connection(&self) -> &Connection {
        self.slot().connection()
    }

    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        self.entry().into_key_value().1
    }
}

impl<'a> Operator<'a> {
    /// Operate some connection.
    ///
    /// This returns `None` if the key does not refer to an existing connection.
    pub(crate) fn new(endpoint: &'a mut dyn Endpoint, key: SlotKey) -> Option<Self> {
        let _ = endpoint.get(key)?;
        Some(Operator {
            endpoint,
            connection_key: key,
        })
    }

    pub(crate) fn from_tuple(endpoint: &'a mut dyn Endpoint, tuple: FourTuple) -> Result<Self, &'a mut dyn Endpoint> {
        let key = match endpoint.find_tuple(tuple) {
            Some(entry) => Some(entry.slot_key()),
            None => None,
        };

        match key {
            Some(key) => Ok(Operator {
                endpoint,
                connection_key: key,
            }),
            None => Err(endpoint),
        }
    }

    pub(crate) fn arrives(&mut self, incoming: &InPacket) -> Signals {
        let (entry_key, connection) = self.entry().into_key_value();
        connection.arrives(incoming, entry_key)
    }

    pub(crate) fn next_send_segment(&mut self, available: AvailableBytes, time: Instant)
        -> OutSignals
    {
        let (entry_key, connection) = self.entry().into_key_value();
        connection.next_send_segment(available, time, entry_key)
    }

    pub(crate) fn open(&mut self, time: Instant) -> Result<(), crate::layer::Error> {
        let (entry_key, connection) = self.entry().into_key_value();
        connection.open(time, entry_key)
    }

    /// Remove the connection and close the operator.
    pub(crate) fn delete(self) -> &'a mut dyn Endpoint {
        self.endpoint.remove(self.connection_key);
        self.endpoint
    }


    fn entry(&mut self) -> Entry {
        self.endpoint.entry(self.connection_key).unwrap()
    }

    fn slot(&self) -> &Slot {
        self.endpoint.get(self.connection_key).unwrap()
    }
}

impl Default for State {
    fn default() -> Self {
        State::Closed
    }
}

impl InnerRepr {
    pub(crate) fn send_back(&self, incoming: &tcp::Repr) -> tcp::Repr {
        self.send_impl(incoming.dst_port, incoming.src_port)
    }

    pub(crate) fn send_to(&self, tuple: FourTuple) -> tcp::Repr {
        self.send_impl(tuple.local_port, tuple.remote_port)
    }

    fn send_impl(&self, src: u16, dst: u16) -> tcp::Repr {
        tcp::Repr {
            src_port: src,
            dst_port: dst,
            seq_number: self.seq_number,
            flags: self.flags,
            ack_number: self.ack_number,
            window_len: self.window_len,
            window_scale: self.window_scale,
            max_seg_size: self.max_seg_size,
            sack_permitted: self.sack_permitted,
            sack_ranges: self.sack_ranges,
            payload_len: self.payload_len,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::layer::tcp::endpoint::{EntryKey, FourTuple, PortMap};
    use crate::layer::tcp::IsnGenerator;
    use crate::time::Instant;
    use crate::wire::ip::Address;
    use super::{AvailableBytes, Connection};

    struct NoRemap;

    impl PortMap for NoRemap {
        fn remap(&mut self, _: FourTuple, _: FourTuple) {
            panic!("Should not get remapped");
        }
    }

    fn simple_connection() -> Connection {
        Connection::zeroed()
    }

    #[test]
    fn resent_syn() {
        let mut connection = simple_connection();
        let isn = IsnGenerator::from_key(0, 0);
        let mut no_remap = NoRemap;
        let mut four = FourTuple {
            local: Address::v4(192, 0, 10, 1),
            remote: Address::v4(192, 0, 10, 2),
            local_port: 80,
            remote_port: 80,
        };

        let time_start = Instant::from_secs(0);
        let time_resend = Instant::from_secs(3);

        let entry = EntryKey::fake(&mut no_remap, &isn, &mut four);
        assert!(connection.open(time_start, entry).is_ok());

        let entry = EntryKey::fake(&mut no_remap, &isn, &mut four);
        let available = AvailableBytes { fin: false, total: 0 };
        let _resent = connection.next_send_segment(available, time_resend, entry);
    }
}
