// Copyright (C) 2018-2019, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::cmp;

use std::collections::hash_map;

use std::collections::BTreeMap;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use crate::Error;
use crate::Result;

use crate::ranges;

const DEFAULT_URGENCY: u8 = 127;

/// Keeps track of QUIC streams and enforces stream limits.
#[derive(Default)]
pub struct StreamMap {
    /// Map of streams indexed by stream ID.
    streams: HashMap<u64, Stream>,

    /// Set of streams that were completed and garbage collected.
    ///
    /// Instead of keeping the full stream state forever, we collect completed
    /// streams to save memory, but we still need to keep track of previously
    /// created streams, to prevent peers from re-creating them.
    collected: HashSet<u64>,

    /// Peer's maximum bidirectional stream count limit.
    peer_max_streams_bidi: u64,

    /// Peer's maximum unidirectional stream count limit.
    peer_max_streams_uni: u64,

    /// The total number of bidirectional streams opened by the peer.
    peer_opened_streams_bidi: u64,

    /// The total number of unidirectional streams opened by the peer.
    peer_opened_streams_uni: u64,

    /// Local maximum bidirectional stream count limit.
    local_max_streams_bidi: u64,
    local_max_streams_bidi_next: u64,

    /// Local maximum unidirectional stream count limit.
    local_max_streams_uni: u64,
    local_max_streams_uni_next: u64,

    /// The total number of bidirectional streams opened by the local endpoint.
    local_opened_streams_bidi: u64,

    /// The total number of unidirectional streams opened by the local endpoint.
    local_opened_streams_uni: u64,

    /// Queue of stream IDs corresponding to streams that have buffered data
    /// ready to be sent to the peer. This also implies that the stream has
    /// enough flow control credits to send at least some of that data.
    ///
    /// Streams are grouped by their priority, where each urgency level has two
    /// queues, one for non-incremental streams and one for incremental ones.
    ///
    /// Streams with lower urgency level are scheduled first, and within the
    /// same urgency level Non-incremental streams are scheduled first, in the
    /// order of their stream IDs, and incremental streams are scheduled in a
    /// round-robin fashion after all non-incremental streams have been flushed.
    flushable: BTreeMap<u8, (BinaryHeap<std::cmp::Reverse<u64>>, VecDeque<u64>)>,

    /// Set of stream IDs corresponding to streams that have outstanding data
    /// to read. This is used to generate a `StreamIter` of streams without
    /// having to iterate over the full list of streams.
    readable: HashSet<u64>,

    /// Set of stream IDs corresponding to streams that have enough flow control
    /// capacity to be written to, and is not finished. This is used to generate
    /// a `StreamIter` of streams without having to iterate over the full list
    /// of streams.
    writable: HashSet<u64>,

    /// Set of stream IDs corresponding to streams that are almost out of flow
    /// control credit and need to send MAX_STREAM_DATA. This is used to
    /// generate a `StreamIter` of streams without having to iterate over the
    /// full list of streams.
    almost_full: HashSet<u64>,

    /// Set of stream IDs corresponding to streams that are blocked. The value
    /// of the map elements represents the offset of the stream at which the
    /// blocking occurred.
    blocked: HashMap<u64, u64>,
}

impl StreamMap {
    pub fn new(max_streams_bidi: u64, max_streams_uni: u64) -> StreamMap {
        StreamMap {
            local_max_streams_bidi: max_streams_bidi,
            local_max_streams_bidi_next: max_streams_bidi,

            local_max_streams_uni: max_streams_uni,
            local_max_streams_uni_next: max_streams_uni,

            ..StreamMap::default()
        }
    }

    /// Returns the stream with the given ID if it exists.
    pub fn get(&self, id: u64) -> Option<&Stream> {
        self.streams.get(&id)
    }

    /// Returns the mutable stream with the given ID if it exists.
    pub fn get_mut(&mut self, id: u64) -> Option<&mut Stream> {
        self.streams.get_mut(&id)
    }

    /// Returns the mutable stream with the given ID if it exists, or creates
    /// a new one otherwise.
    ///
    /// The `local` parameter indicates whether the stream's creation was
    /// requested by the local application rather than the peer, and is
    /// used to validate the requested stream ID, and to select the initial
    /// flow control values from the local and remote transport parameters
    /// (also passed as arguments).
    ///
    /// This also takes care of enforcing both local and the peer's stream
    /// count limits. If one of these limits is violated, the `StreamLimit`
    /// error is returned.
    pub(crate) fn get_or_create(
        &mut self, id: u64, local_params: &crate::TransportParams,
        peer_params: &crate::TransportParams, local: bool, is_server: bool,
    ) -> Result<&mut Stream> {
        let stream = match self.streams.entry(id) {
            hash_map::Entry::Vacant(v) => {
                // Stream has already been closed and garbage collected.
                if self.collected.contains(&id) {
                    return Err(Error::Done);
                }

                if local != is_local(id, is_server) {
                    return Err(Error::InvalidStreamState);
                }

                let (max_rx_data, max_tx_data) = match (local, is_bidi(id)) {
                    // Locally-initiated bidirectional stream.
                    (true, true) => (
                        local_params.initial_max_stream_data_bidi_local,
                        peer_params.initial_max_stream_data_bidi_remote,
                    ),

                    // Locally-initiated unidirectional stream.
                    (true, false) => (0, peer_params.initial_max_stream_data_uni),

                    // Remotely-initiated bidirectional stream.
                    (false, true) => (
                        local_params.initial_max_stream_data_bidi_remote,
                        peer_params.initial_max_stream_data_bidi_local,
                    ),

                    // Remotely-initiated unidirectional stream.
                    (false, false) =>
                        (local_params.initial_max_stream_data_uni, 0),
                };

                // Enforce stream count limits.
                match (is_local(id, is_server), is_bidi(id)) {
                    (true, true) => {
                        if self.local_opened_streams_bidi >=
                            self.peer_max_streams_bidi
                        {
                            return Err(Error::StreamLimit);
                        }

                        self.local_opened_streams_bidi += 1;
                    },

                    (true, false) => {
                        if self.local_opened_streams_uni >=
                            self.peer_max_streams_uni
                        {
                            return Err(Error::StreamLimit);
                        }

                        self.local_opened_streams_uni += 1;
                    },

                    (false, true) => {
                        if self.peer_opened_streams_bidi >=
                            self.local_max_streams_bidi
                        {
                            return Err(Error::StreamLimit);
                        }

                        self.peer_opened_streams_bidi += 1;
                    },

                    (false, false) => {
                        if self.peer_opened_streams_uni >=
                            self.local_max_streams_uni
                        {
                            return Err(Error::StreamLimit);
                        }

                        self.peer_opened_streams_uni += 1;
                    },
                };

                let s = Stream::new(max_rx_data, max_tx_data, is_bidi(id), local);
                v.insert(s)
            },

            hash_map::Entry::Occupied(v) => v.into_mut(),
        };

        // Stream might already be writable due to initial flow control limits.
        if stream.is_writable() {
            self.writable.insert(id);
        }

        Ok(stream)
    }

    /// Pushes the stream ID to the back of the flushable streams queue with
    /// the specified urgency.
    ///
    /// Note that the caller is responsible for checking that the specified
    /// stream ID was not in the queue already before calling this.
    ///
    /// Queueing a stream multiple times simultaneously means that it might be
    /// unfairly scheduled more often than other streams, and might also cause
    /// spurious cycles through the queue, so it should be avoided.
    pub fn push_flushable(&mut self, stream_id: u64, urgency: u8, incr: bool) {
        // Push the element to the back of the queue corresponding to the given
        // urgency. If the queue doesn't exist yet, create it first.
        let queues = self
            .flushable
            .entry(urgency)
            .or_insert_with(|| (BinaryHeap::new(), VecDeque::new()));

        if !incr {
            // Non-incremental streams are scheduled in order of their stream ID.
            queues.0.push(std::cmp::Reverse(stream_id))
        } else {
            // Incremental streams are scheduled in a round-robin fashion.
            queues.1.push_back(stream_id)
        };
    }

    /// Removes and returns the first stream ID from the flushable streams
    /// queue with the specified urgency.
    ///
    /// Note that if the stream is still flushable after sending some of its
    /// outstanding data, it needs to be added back to the queue.
    pub fn pop_flushable(&mut self) -> Option<u64> {
        // Remove the first element from the queue corresponding to the lowest
        // urgency that has elements.
        let (node, clear) =
            if let Some((urgency, queues)) = self.flushable.iter_mut().next() {
                let node = if !queues.0.is_empty() {
                    queues.0.pop().map(|x| x.0)
                } else {
                    queues.1.pop_front()
                };

                let clear = if queues.0.is_empty() && queues.1.is_empty() {
                    Some(*urgency)
                } else {
                    None
                };

                (node, clear)
            } else {
                (None, None)
            };

        // Remove the queue from the list of queues if it is now empty, so that
        // the next time `pop_flushable()` is called the next queue with elements
        // is used.
        if let Some(urgency) = &clear {
            self.flushable.remove(urgency);
        }

        node
    }

    /// Adds or removes the stream ID to/from the readable streams set.
    ///
    /// If the stream was already in the list, this does nothing.
    pub fn mark_readable(&mut self, stream_id: u64, readable: bool) {
        if readable {
            self.readable.insert(stream_id);
        } else {
            self.readable.remove(&stream_id);
        }
    }

    /// Adds or removes the stream ID to/from the writable streams set.
    ///
    /// This should also be called anytime a new stream is created, in addition
    /// to when an existing stream becomes writable (or stops being writable).
    ///
    /// If the stream was already in the list, this does nothing.
    pub fn mark_writable(&mut self, stream_id: u64, writable: bool) {
        if writable {
            self.writable.insert(stream_id);
        } else {
            self.writable.remove(&stream_id);
        }
    }

    /// Adds or removes the stream ID to/from the almost full streams set.
    ///
    /// If the stream was already in the list, this does nothing.
    pub fn mark_almost_full(&mut self, stream_id: u64, almost_full: bool) {
        if almost_full {
            self.almost_full.insert(stream_id);
        } else {
            self.almost_full.remove(&stream_id);
        }
    }

    /// Adds or removes the stream ID to/from the blocked streams set with the
    /// given offset value.
    ///
    /// If the stream was already in the list, this does nothing.
    pub fn mark_blocked(&mut self, stream_id: u64, blocked: bool, off: u64) {
        if blocked {
            self.blocked.insert(stream_id, off);
        } else {
            self.blocked.remove(&stream_id);
        }
    }

    /// Updates the peer's maximum bidirectional stream count limit.
    pub fn update_peer_max_streams_bidi(&mut self, v: u64) {
        self.peer_max_streams_bidi = cmp::max(self.peer_max_streams_bidi, v);
    }

    /// Updates the peer's maximum unidirectional stream count limit.
    pub fn update_peer_max_streams_uni(&mut self, v: u64) {
        self.peer_max_streams_uni = cmp::max(self.peer_max_streams_uni, v);
    }

    /// Commits the new max_streams_bidi limit.
    pub fn update_max_streams_bidi(&mut self) {
        self.local_max_streams_bidi = self.local_max_streams_bidi_next;
    }

    /// Returns the new max_streams_bidi limit.
    pub fn max_streams_bidi_next(&mut self) -> u64 {
        self.local_max_streams_bidi_next
    }

    /// Commits the new max_streams_uni limit.
    pub fn update_max_streams_uni(&mut self) {
        self.local_max_streams_uni = self.local_max_streams_uni_next;
    }

    /// Returns the new max_streams_uni limit.
    pub fn max_streams_uni_next(&mut self) -> u64 {
        self.local_max_streams_uni_next
    }

    /// Drops completed stream.
    ///
    /// This should only be called when Stream::is_complete() returns true for
    /// the given stream.
    pub fn collect(&mut self, stream_id: u64, local: bool) {
        if !local {
            // If the stream was created by the peer, give back a max streams
            // credit.
            if is_bidi(stream_id) {
                self.local_max_streams_bidi_next =
                    self.local_max_streams_bidi_next.saturating_add(1);
            } else {
                self.local_max_streams_uni_next =
                    self.local_max_streams_uni_next.saturating_add(1);
            }
        }

        self.streams.remove(&stream_id);
        self.collected.insert(stream_id);
    }

    /// Creates an iterator over streams that have outstanding data to read.
    pub fn readable(&self) -> StreamIter {
        StreamIter::from(&self.readable)
    }

    /// Creates an iterator over streams that can be written to.
    pub fn writable(&self) -> StreamIter {
        StreamIter::from(&self.writable)
    }

    /// Creates an iterator over streams that need to send MAX_STREAM_DATA.
    pub fn almost_full(&self) -> StreamIter {
        StreamIter::from(&self.almost_full)
    }

    /// Creates an iterator over streams that need to send STREAM_DATA_BLOCKED.
    pub fn blocked(&self) -> hash_map::Iter<u64, u64> {
        self.blocked.iter()
    }

    /// Returns true if there are any streams that have data to write.
    pub fn has_flushable(&self) -> bool {
        !self.flushable.is_empty()
    }

    /// Returns true if there are any streams that need to update the local
    /// flow control limit.
    pub fn has_almost_full(&self) -> bool {
        !self.almost_full.is_empty()
    }

    /// Returns true if there are any streams that are blocked.
    pub fn has_blocked(&self) -> bool {
        !self.blocked.is_empty()
    }

    /// Returns true if the max bidirectional streams count needs to be updated
    /// by sending a MAX_STREAMS frame to the peer.
    pub fn should_update_max_streams_bidi(&self) -> bool {
        self.local_max_streams_bidi_next != self.local_max_streams_bidi &&
            self.local_max_streams_bidi_next / 2 >
                self.local_max_streams_bidi - self.peer_opened_streams_bidi
    }

    /// Returns true if the max unidirectional streams count needs to be updated
    /// by sending a MAX_STREAMS frame to the peer.
    pub fn should_update_max_streams_uni(&self) -> bool {
        self.local_max_streams_uni_next != self.local_max_streams_uni &&
            self.local_max_streams_uni_next / 2 >
                self.local_max_streams_uni - self.peer_opened_streams_uni
    }

    /// Returns the number of active streams in the map.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.streams.len()
    }
}

/// A QUIC stream.
#[derive(Default)]
pub struct Stream {
    /// Receive-side stream buffer.
    pub recv: RecvBuf,

    /// Send-side stream buffer.
    pub send: SendBuf,

    /// Whether the stream is bidirectional.
    pub bidi: bool,

    /// Whether the stream was created by the local endpoint.
    pub local: bool,

    /// Application data.
    pub data: Option<Box<dyn Send + std::any::Any>>,

    /// The stream's urgency (lower is better). Default is `DEFAULT_URGENCY`.
    pub urgency: u8,

    /// Whether the stream can be flushed incrementally. Default is `true`.
    pub incremental: bool,
}

impl Stream {
    /// Creates a new stream with the given flow control limits.
    pub fn new(
        max_rx_data: u64, max_tx_data: u64, bidi: bool, local: bool,
    ) -> Stream {
        Stream {
            recv: RecvBuf::new(max_rx_data),
            send: SendBuf::new(max_tx_data),
            bidi,
            local,
            data: None,
            urgency: DEFAULT_URGENCY,
            incremental: true,
        }
    }

    /// Returns true if the stream has data to read.
    pub fn is_readable(&self) -> bool {
        self.recv.ready()
    }

    /// Returns true if the stream has enough flow control capacity to be
    /// written to, and is not finished.
    pub fn is_writable(&self) -> bool {
        !self.send.shutdown &&
            !self.send.is_fin() &&
            self.send.off < self.send.max_data
    }

    /// Returns true if the stream has data to send and is allowed to send at
    /// least some of it.
    pub fn is_flushable(&self) -> bool {
        self.send.ready() && self.send.off_front() < self.send.max_data
    }

    /// Returns true if the stream is complete.
    ///
    /// For bidirectional streams this happens when both the receive and send
    /// sides are complete. That is when all incoming data has been read by the
    /// application, and when all outgoing data has been acked by the peer.
    ///
    /// For unidirectional streams this happens when either the receive or send
    /// side is complete, depending on whether the stream was created locally
    /// or not.
    pub fn is_complete(&self) -> bool {
        match (self.bidi, self.local) {
            // For bidirectional streams we need to check both receive and send
            // sides for completion.
            (true, _) => self.recv.is_fin() && self.send.is_complete(),

            // For unidirectional streams generated locally, we only need to
            // check the send side for completion.
            (false, true) => self.send.is_complete(),

            // For unidirectional streams generated by the peer, we only need
            // to check the receive side for completion.
            (false, false) => self.recv.is_fin(),
        }
    }
}

/// Returns true if the stream was created locally.
pub fn is_local(stream_id: u64, is_server: bool) -> bool {
    (stream_id & 0x1) == (is_server as u64)
}

/// Returns true if the stream is bidirectional.
pub fn is_bidi(stream_id: u64) -> bool {
    (stream_id & 0x2) == 0
}

/// An iterator over QUIC streams.
#[derive(Default)]
pub struct StreamIter {
    streams: Vec<u64>,
}

impl StreamIter {
    fn from(streams: &HashSet<u64>) -> Self {
        StreamIter {
            streams: streams.iter().copied().collect(),
        }
    }
}

impl Iterator for StreamIter {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        self.streams.pop()
    }
}

impl ExactSizeIterator for StreamIter {
    fn len(&self) -> usize {
        self.streams.len()
    }
}

/// Receive-side stream buffer.
///
/// Stream data received by the peer is buffered in a list of data chunks
/// ordered by offset in ascending order. Contiguous data can then be read
/// into a slice.
#[derive(Debug, Default)]
pub struct RecvBuf {
    /// Chunks of data received from the peer that have not yet been read by
    /// the application, ordered by offset.
    data: BinaryHeap<RangeBuf>,

    /// The lowest data offset that has yet to be read by the application.
    off: u64,

    /// The total length of data received on this stream.
    len: u64,

    /// The maximum offset the peer is allowed to send us.
    max_data: u64,

    /// The updated maximum offset the peer is allowed to send us.
    max_data_next: u64,

    /// The final stream offset received from the peer, if any.
    fin_off: Option<u64>,

    /// Whether incoming data is validated but not buffered.
    drain: bool,
}

impl RecvBuf {
    /// Creates a new receive buffer.
    fn new(max_data: u64) -> RecvBuf {
        RecvBuf {
            max_data,
            max_data_next: max_data,
            ..RecvBuf::default()
        }
    }

    /// Inserts the given chunk of data in the buffer.
    ///
    /// This also takes care of enforcing stream flow control limits, as well
    /// as handling incoming data that overlaps data that is already in the
    /// buffer.
    pub fn write(&mut self, buf: RangeBuf) -> Result<()> {
        if buf.max_off() > self.max_data {
            return Err(Error::FlowControl);
        }

        if let Some(fin_off) = self.fin_off {
            // Stream's size is known, forbid data beyond that point.
            if buf.max_off() > fin_off {
                return Err(Error::FinalSize);
            }

            // Stream's size is already known, forbid changing it.
            if buf.fin() && fin_off != buf.max_off() {
                return Err(Error::FinalSize);
            }
        }

        // Stream's known size is lower than data already received.
        if buf.fin() && buf.max_off() < self.len {
            return Err(Error::FinalSize);
        }

        // We already saved the final offset, so there's nothing else we
        // need to keep from the RangeBuf if it's empty.
        if self.fin_off.is_some() && buf.is_empty() {
            return Ok(());
        }

        // No need to process an empty buffer with the fin flag, if we already
        // know the final size.
        if buf.fin() && buf.is_empty() && self.fin_off.is_some() {
            return Ok(());
        }

        if buf.fin() {
            self.fin_off = Some(buf.max_off());
        }

        // No need to store empty buffer that doesn't carry the fin flag.
        if !buf.fin() && buf.is_empty() {
            return Ok(());
        }

        // Check if data is fully duplicate, that is the buffer's max offset is
        // lower or equal to the offset already stored in the recv buffer.
        if self.off >= buf.max_off() {
            // An exception is applied to empty range buffers, because an empty
            // buffer's max offset matches the max offset of the recv buffer.
            //
            // By this point all spurious empty buffers should have already been
            // discarded, so allowing empty buffers here should be safe.
            if !buf.is_empty() {
                return Ok(());
            }
        }

        if self.drain {
            return Ok(());
        }

        let mut tmp_buf = Some(buf);

        while let Some(mut buf) = tmp_buf {
            tmp_buf = None;

            // Discard incoming data below current stream offset. Bytes up to
            // `self.off` have already been received so we should not buffer
            // them again. This is also important to make sure `ready()` doesn't
            // get stuck when a buffer with lower offset than the stream's is
            // buffered.
            if self.off > buf.off() {
                buf = buf.split_off((self.off - buf.off()) as usize);
            }

            for b in &self.data {
                // New buffer is fully contained in existing buffer.
                if buf.off() >= b.off() && buf.max_off() <= b.max_off() {
                    return Ok(());
                }

                // New buffer's start overlaps existing buffer.
                if buf.off() >= b.off() && buf.off() < b.max_off() {
                    buf = buf.split_off((b.max_off() - buf.off()) as usize);
                }

                // New buffer's end overlaps existing buffer.
                if buf.off() < b.off() && buf.max_off() > b.off() {
                    tmp_buf = Some(buf.split_off((b.off() - buf.off()) as usize));
                }
            }

            self.len = cmp::max(self.len, buf.max_off());

            self.data.push(buf);
        }

        Ok(())
    }

    /// Writes data from the receive buffer into the given output buffer.
    ///
    /// Only contiguous data is written to the output buffer, starting from
    /// offset 0. The offset is incremented as data is read out of the receive
    /// buffer into the application buffer. If there is no data at the expected
    /// read offset, the `Done` error is returned.
    ///
    /// On success the amount of data read, and a flag indicating if there is
    /// no more data in the buffer, are returned as a tuple.
    pub fn emit(&mut self, out: &mut [u8]) -> Result<(usize, bool)> {
        let mut len = 0;
        let mut cap = out.len();

        if !self.ready() {
            return Err(Error::Done);
        }

        while cap > 0 && self.ready() {
            let mut buf = match self.data.peek_mut() {
                Some(v) => v,

                None => break,
            };

            let buf_len = cmp::min(buf.len(), cap);

            out[len..len + buf_len].copy_from_slice(&buf[..buf_len]);

            self.off += buf_len as u64;

            len += buf_len;
            cap -= buf_len;

            if buf_len < buf.len() {
                buf.consume(buf_len);

                // We reached the maximum capacity, so end here.
                break;
            }

            std::collections::binary_heap::PeekMut::pop(buf);
        }

        self.max_data_next = self.max_data_next.saturating_add(len as u64);

        Ok((len, self.is_fin()))
    }

    /// Resets the stream at the given offset.
    pub fn reset(&mut self, final_size: u64) -> Result<usize> {
        // Stream's size is already known, forbid changing it.
        if let Some(fin_off) = self.fin_off {
            if fin_off != final_size {
                return Err(Error::FinalSize);
            }
        }

        // Stream's known size is lower than data already received.
        if final_size < self.len {
            return Err(Error::FinalSize);
        }

        self.fin_off = Some(final_size);

        // Return how many bytes need to be removed from the connection flow
        // control.
        Ok((final_size - self.len) as usize)
    }

    /// Commits the new max_data limit.
    pub fn update_max_data(&mut self) {
        self.max_data = self.max_data_next;
    }

    /// Return the new max_data limit.
    pub fn max_data_next(&mut self) -> u64 {
        self.max_data_next
    }

    /// Shuts down receiving data.
    pub fn shutdown(&mut self) -> Result<()> {
        if self.drain {
            return Err(Error::Done);
        }

        self.drain = true;

        self.data.clear();

        Ok(())
    }

    /// Returns the lowest offset of data buffered.
    #[allow(dead_code)]
    pub fn off_front(&self) -> u64 {
        self.off
    }

    /// Returns true if we need to update the local flow control limit.
    pub fn almost_full(&self) -> bool {
        // Send MAX_STREAM_DATA when the new limit is at least double the
        // amount of data that can be received before blocking.
        self.fin_off.is_none() &&
            self.max_data_next != self.max_data &&
            self.max_data_next / 2 > self.max_data - self.len
    }

    /// Returns the largest offset ever received.
    pub fn max_off(&self) -> u64 {
        self.len
    }

    /// Returns true if the receive-side of the stream is complete.
    ///
    /// This happens when the stream's receive final size is known, and the
    /// application has read all data from the stream.
    pub fn is_fin(&self) -> bool {
        if self.fin_off == Some(self.off) {
            return true;
        }

        false
    }

    /// Returns true if the stream has data to be read.
    fn ready(&self) -> bool {
        let buf = match self.data.peek() {
            Some(v) => v,

            None => return false,
        };

        buf.off() == self.off
    }
}

/// Send-side stream buffer.
///
/// Stream data scheduled to be sent to the peer is buffered in a list of data
/// chunks ordered by offset in ascending order. Contiguous data can then be
/// read into a slice.
///
/// By default, new data is appended at the end of the stream, but data can be
/// inserted at the start of the buffer (this is to allow data that needs to be
/// retransmitted to be re-buffered).
#[derive(Debug, Default)]
pub struct SendBuf {
    /// Chunks of data to be sent, ordered by offset.
    data: VecDeque<RangeBuf>,

    pos: usize,

    /// The maximum offset of data buffered in the stream.
    off: u64,

    /// The amount of data that was ever written to this stream.
    len: u64,

    /// The maximum offset we are allowed to send to the peer.
    max_data: u64,

    /// The final stream offset written to the stream, if any.
    fin_off: Option<u64>,

    /// Whether the stream's send-side has been shut down.
    shutdown: bool,

    /// Ranges of data offsets that have been acked.
    acked: ranges::RangeSet,
}

impl SendBuf {
    /// Creates a new send buffer.
    fn new(max_data: u64) -> SendBuf {
        SendBuf {
            max_data,
            ..SendBuf::default()
        }
    }

    /// Inserts the given slice of data at the end of the buffer.
    ///
    /// The number of bytes that were actually stored in the buffer is returned
    /// (this may be lower than the size of the input buffer, in case of partial
    /// writes).
    pub fn push_slice(
        &mut self, mut data: &[u8], mut fin: bool,
    ) -> Result<usize> {
        if self.shutdown {
            // Since we won't write any more data anyway, pretend that we sent
            // all data that was passed in.
            return Ok(data.len());
        }

        if data.is_empty() {
            // Create a dummy range buffer, in order to propagate the `fin` flag
            // into `RangeBuf::push()`. This will be discarded later on.
            let buf = RangeBuf::from(&[], self.off, fin);

            return self.push(buf).map(|_| 0);
        }

        if data.len() > self.cap() {
            // Truncate the input buffer according to the stream's capacity.
            let len = self.cap();
            data = &data[..len];

            // We are not buffering the full input, so clear the fin flag.
            fin = false;
        }

        let buf = RangeBuf::from(data, self.off, fin);
        self.push(buf)?;

        self.off += data.len() as u64;

        Ok(data.len())
    }

    /// Inserts the given chunk of data in the buffer.
    pub fn push(&mut self, buf: RangeBuf) -> Result<()> {
        if let Some(fin_off) = self.fin_off {
            // Can't write past final offset.
            if buf.max_off() > fin_off {
                return Err(Error::FinalSize);
            }

            // Can't "undo" final offset.
            if buf.max_off() == fin_off && !buf.fin() {
                return Err(Error::FinalSize);
            }
        }

        if self.shutdown {
            return Ok(());
        }

        if buf.fin() {
            self.fin_off = Some(buf.max_off());
        }

        // Don't queue data that was already fully acked.
        if self.ack_off() >= buf.max_off() {
            return Ok(());
        }

        self.len += buf.len() as u64;

        // We already recorded the final offset, so we can just discard the
        // empty buffer now.
        if buf.is_empty() {
            return Ok(());
        }

        match self.data.back() {
            None => self.data.push_back(buf),

            Some(back) =>
                if buf.off >= back.max_off() {
                    self.data.push_back(buf);
                } else {
                    let mut insert_at = None;

                    for i in 0..self.data.len() {
                        if buf.off < self.data[i].off {
                            insert_at = Some(i);
                            break;
                        }
                    }

                    match insert_at {
                        Some(insert_at) => self.data.insert(insert_at, buf),

                        None => self.data.push_back(buf),
                    }
                },
        }

        Ok(())
    }

    /// Returns contiguous data from the send buffer as a single `RangeBuf`.
    pub fn pop(&mut self, max_data: usize) -> Result<RangeBuf> {
        let mut out = RangeBuf::default();
        out.data =
            Vec::with_capacity(cmp::min(max_data as u64, self.len) as usize);
        out.off = self.off;

        let mut out_len = max_data;
        let mut out_off = self.off_front();

        while out_len > 0 &&
            self.ready() &&
            self.off_front() == out_off &&
            self.off_front() < self.max_data
        {
            let buf = match self.data.front_mut() {
                Some(v) => v,

                None => break,
            };

            let buf_len = cmp::min(buf.len(), out_len);

            if out.is_empty() {
                out.off = buf.off();
            }

            self.len -= buf_len as u64;

            out_len -= buf_len;
            out_off = buf.off() + buf_len as u64;

            out.data.extend_from_slice(&buf[..buf_len]);

            if buf_len < buf.len() {
                buf.consume(buf_len);

                // We reached the maximum capacity, so end here.
                break;
            }

            self.data.pop_front();
        }

        // Override the `fin` flag set for the output buffer by matching the
        // buffer's maximum offset against the stream's final offset (if known).
        //
        // This is more efficient than tracking `fin` using the range buffers
        // themselves, and lets us avoid queueing empty buffers just so we can
        // propagate the final size.
        out.fin = self.fin_off == Some(out.max_off());

        Ok(out)
    }

    /// Writes data from the send buffer into the given output buffer.
    pub fn emit(&mut self, out: &mut [u8]) -> Result<(usize, bool)> {
        let mut out_len = out.len();
        let out_off = self.off_front();

        let mut next_off = out_off;

        while out_len > 0 &&
            self.ready() &&
            self.off_front() == next_off &&
            self.off_front() < self.max_data
        {
            let buf = match self.data.get_mut(self.pos) {
                Some(v) => v,

                None => break,
            };

            let buf_len = cmp::min(buf.len(), out_len);

            let out_pos = (next_off - out_off) as usize;
            (&mut out[out_pos..out_pos + buf_len])
                .copy_from_slice(&buf[..buf_len]);

            self.len -= buf_len as u64;

            out_len -= buf_len;

            next_off = buf.off() + buf_len as u64;

            if buf_len < buf.len() {
                buf.consume(buf_len);

                // We reached the maximum capacity, so end here.
                break;
            }

            buf.consume(buf_len);

            self.pos += 1;
        }

        // Override the `fin` flag set for the output buffer by matching the
        // buffer's maximum offset against the stream's final offset (if known).
        //
        // This is more efficient than tracking `fin` using the range buffers
        // themselves, and lets us avoid queueing empty buffers just so we can
        // propagate the final size.
        let fin = self.fin_off == Some(next_off);

        Ok((out.len() - out_len, fin))
    }

    /// Updates the max_data limit to the given value.
    pub fn update_max_data(&mut self, max_data: u64) {
        self.max_data = cmp::max(self.max_data, max_data);
    }

    /// Increments the acked data offset.
    pub fn ack(&mut self, off: u64, len: usize) {
        self.acked.insert(off..off + len as u64);
    }

    pub fn ack_and_drop(&mut self, off: u64, len: usize) {
        self.ack(off, len);

        let ack_off = self.ack_off();

        if self.data.is_empty() || off > self.data[0].max_off() {
            return;
        }

        if off > ack_off {
            return;
        }

        let mut drop_until = None;

        for (i, buf) in self.data.iter_mut().enumerate() {
            if buf.off >= ack_off {
                break;
            }

            if buf.off < ack_off && ack_off < buf.max_off() {
                break;
            }

            drop_until = Some(i);
        }

        if let Some(drop) = drop_until {
            self.data.drain(..=drop);

            self.pos -= drop + 1;
        }
    }

    pub fn retransmit(&mut self, off: u64, len: usize) {
        let max_off = off + len as u64;

        if self.data.is_empty() {
            return;
        }

        for (i, buf) in self.data.iter_mut().enumerate() {
            if max_off < buf.off {
                break;
            }

            if off > buf.max_off() {
                continue;
            }

            buf.pos = if off > buf.off {
                cmp::min(buf.pos, (off - buf.off) as usize)
            } else {
                0
            };

            self.pos = cmp::min(self.pos, i);

            self.len += buf.len() as u64;
        }
    }

    /// Shuts down sending data.
    pub fn shutdown(&mut self) -> Result<()> {
        if self.shutdown {
            return Err(Error::Done);
        }

        self.shutdown = true;

        self.data.clear();

        Ok(())
    }

    /// Returns the largest offset of data buffered.
    #[allow(dead_code)]
    pub fn off_back(&self) -> u64 {
        self.off
    }

    /// Returns the lowest offset of data buffered.
    pub fn off_front(&self) -> u64 {
        match self.data.get(self.pos) {
            Some(v) => v.off(),

            None => self.off,
        }
    }

    /// The maximum offset we are allowed to send to the peer.
    pub fn max_off(&self) -> u64 {
        self.max_data
    }

    /// Returns true if all data in the stream has been sent.
    ///
    /// This happens when the stream's send final size is knwon, and the
    /// application has already written data up to that point.
    pub fn is_fin(&self) -> bool {
        if self.fin_off == Some(self.off) {
            return true;
        }

        false
    }

    /// Returns true if the send-side of the stream is complete.
    ///
    /// This happens when the stream's send final size is known, and the peer
    /// has already acked all stream data up to that point.
    pub fn is_complete(&self) -> bool {
        if let Some(fin_off) = self.fin_off {
            if self.acked == (0..fin_off) {
                return true;
            }
        }

        false
    }

    /// Returns true if there is data to be written.
    fn ready(&self) -> bool {
        !self.data.is_empty() && self.off_front() < self.off
    }

    /// Returns the highest contiguously acked offset.
    fn ack_off(&self) -> u64 {
        match self.acked.iter().next() {
            // Only consider the initial range if it contiguously covers the
            // start of the stream (i.e. from offset 0).
            Some(std::ops::Range { start: 0, end }) => end,

            Some(_) | None => 0,
        }
    }

    /// Returns the outgoing flow control capacity.
    pub fn cap(&self) -> usize {
        (self.max_data - self.off) as usize
    }
}

/// Buffer holding data at a specific offset.
#[derive(Clone, Debug, Default, Eq)]
pub struct RangeBuf {
    /// The internal buffer holding the data.
    data: Vec<u8>,

    /// The starting offset within `data`. This allows partially consuming a
    /// buffer without duplicating the data.
    pos: usize,

    /// The starting offset within a stream.
    off: u64,

    /// Whether this contains the final byte in the stream.
    fin: bool,
}

impl RangeBuf {
    /// Creates a new `RangeBuf` from the given slice.
    pub(crate) fn from(buf: &[u8], off: u64, fin: bool) -> RangeBuf {
        RangeBuf {
            data: Vec::from(buf),
            pos: 0,
            off,
            fin,
        }
    }

    /// Returns whether `self` holds the final offset in the stream.
    pub fn fin(&self) -> bool {
        self.fin
    }

    /// Returns the starting offset of `self`.
    pub fn off(&self) -> u64 {
        self.off + self.pos as u64
    }

    /// Returns the final offset of `self`.
    pub fn max_off(&self) -> u64 {
        self.off() + self.len() as u64
    }

    /// Returns the length of `self`.
    pub fn len(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Returns true if `self` has a length of zero bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Consumes the starting `count` bytes of `self`.
    pub fn consume(&mut self, count: usize) {
        self.pos += count;
    }

    /// Splits the buffer into two at the given index.
    pub fn split_off(&mut self, at: usize) -> RangeBuf {
        let buf = RangeBuf {
            data: self.data.split_off(at),
            pos: 0,
            off: self.off + at as u64,
            fin: self.fin,
        };

        self.fin = false;

        buf
    }
}

impl std::ops::Deref for RangeBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data[self.pos..]
    }
}

impl std::ops::DerefMut for RangeBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.pos..]
    }
}

impl Ord for RangeBuf {
    fn cmp(&self, other: &RangeBuf) -> cmp::Ordering {
        // Invert ordering to implement min-heap.
        self.off.cmp(&other.off).reverse()
    }
}

impl PartialOrd for RangeBuf {
    fn partial_cmp(&self, other: &RangeBuf) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RangeBuf {
    fn eq(&self, other: &RangeBuf) -> bool {
        self.off == other.off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn empty_stream_frame() {
        let mut recv = RecvBuf::new(15);
        assert_eq!(recv.len, 0);

        let buf = RangeBuf::from(b"hello", 0, false);
        assert!(recv.write(buf).is_ok());
        assert_eq!(recv.len, 5);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        let mut buf = [0; 32];
        assert_eq!(recv.emit(&mut buf), Ok((5, false)));

        // Don't store non-fin empty buffer.
        let buf = RangeBuf::from(b"", 10, false);
        assert!(recv.write(buf).is_ok());
        assert_eq!(recv.len, 5);
        assert_eq!(recv.off, 5);
        assert_eq!(recv.data.len(), 0);

        // Check flow control for empty buffer.
        let buf = RangeBuf::from(b"", 16, false);
        assert_eq!(recv.write(buf), Err(Error::FlowControl));

        // Store fin empty buffer.
        let buf = RangeBuf::from(b"", 5, true);
        assert!(recv.write(buf).is_ok());
        assert_eq!(recv.len, 5);
        assert_eq!(recv.off, 5);
        assert_eq!(recv.data.len(), 1);

        // Don't store additional fin empty buffers.
        let buf = RangeBuf::from(b"", 5, true);
        assert!(recv.write(buf).is_ok());
        assert_eq!(recv.len, 5);
        assert_eq!(recv.off, 5);
        assert_eq!(recv.data.len(), 1);

        // Don't store additional fin non-empty buffers.
        let buf = RangeBuf::from(b"aa", 3, true);
        assert!(recv.write(buf).is_ok());
        assert_eq!(recv.len, 5);
        assert_eq!(recv.off, 5);
        assert_eq!(recv.data.len(), 1);

        // Validate final size with fin empty buffers.
        let buf = RangeBuf::from(b"", 6, true);
        assert_eq!(recv.write(buf), Err(Error::FinalSize));
        let buf = RangeBuf::from(b"", 4, true);
        assert_eq!(recv.write(buf), Err(Error::FinalSize));

        let mut buf = [0; 32];
        assert_eq!(recv.emit(&mut buf), Ok((0, true)));
    }

    #[test]
    fn ordered_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"hello", 0, false);
        let second = RangeBuf::from(b"world", 5, false);
        let third = RangeBuf::from(b"something", 10, true);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 10);
        assert_eq!(recv.off, 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));

        assert!(recv.write(third).is_ok());
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 0);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 19);
        assert_eq!(fin, true);
        assert_eq!(&buf[..len], b"helloworldsomething");
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 19);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn split_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"something", 0, false);
        let second = RangeBuf::from(b"helloworld", 9, true);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 0);

        let (len, fin) = recv.emit(&mut buf[..10]).unwrap();
        assert_eq!(len, 10);
        assert_eq!(fin, false);
        assert_eq!(&buf[..len], b"somethingh");
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 10);

        let (len, fin) = recv.emit(&mut buf[..5]).unwrap();
        assert_eq!(len, 5);
        assert_eq!(fin, false);
        assert_eq!(&buf[..len], b"ellow");
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 15);

        let (len, fin) = recv.emit(&mut buf[..10]).unwrap();
        assert_eq!(len, 4);
        assert_eq!(fin, true);
        assert_eq!(&buf[..len], b"orld");
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 19);
    }

    #[test]
    fn incomplete_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"something", 0, false);
        let second = RangeBuf::from(b"helloworld", 9, true);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 0);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 19);
        assert_eq!(fin, true);
        assert_eq!(&buf[..len], b"somethinghelloworld");
        assert_eq!(recv.len, 19);
        assert_eq!(recv.off, 19);
    }

    #[test]
    fn zero_len_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"something", 0, false);
        let second = RangeBuf::from(b"", 9, true);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 9);
        assert_eq!(fin, true);
        assert_eq!(&buf[..len], b"something");
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 9);
    }

    #[test]
    fn past_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"something", 0, false);
        let second = RangeBuf::from(b"hello", 3, false);
        let third = RangeBuf::from(b"ello", 4, true);
        let fourth = RangeBuf::from(b"ello", 5, true);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 9);
        assert_eq!(fin, false);
        assert_eq!(&buf[..len], b"something");
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 9);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 9);
        assert_eq!(recv.data.len(), 0);

        assert_eq!(recv.write(third), Err(Error::FinalSize));

        assert!(recv.write(fourth).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 9);
        assert_eq!(recv.data.len(), 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn fully_overlapping_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"something", 0, false);
        let second = RangeBuf::from(b"hello", 4, false);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 9);
        assert_eq!(fin, false);
        assert_eq!(&buf[..len], b"something");
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 9);
        assert_eq!(recv.data.len(), 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn fully_overlapping_read2() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"something", 0, false);
        let second = RangeBuf::from(b"hello", 4, false);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 2);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 9);
        assert_eq!(fin, false);
        assert_eq!(&buf[..len], b"somehello");
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 9);
        assert_eq!(recv.data.len(), 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn fully_overlapping_read3() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"something", 0, false);
        let second = RangeBuf::from(b"hello", 3, false);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 8);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 3);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 9);
        assert_eq!(fin, false);
        assert_eq!(&buf[..len], b"somhellog");
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 9);
        assert_eq!(recv.data.len(), 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn fully_overlapping_read_multi() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"somethingsomething", 0, false);
        let second = RangeBuf::from(b"hello", 3, false);
        let third = RangeBuf::from(b"hello", 12, false);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 8);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(third).is_ok());
        assert_eq!(recv.len, 17);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 2);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 18);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 5);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 18);
        assert_eq!(fin, false);
        assert_eq!(&buf[..len], b"somhellogsomhellog");
        assert_eq!(recv.len, 18);
        assert_eq!(recv.off, 18);
        assert_eq!(recv.data.len(), 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn overlapping_start_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"something", 0, false);
        let second = RangeBuf::from(b"hello", 8, true);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 13);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 2);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 13);
        assert_eq!(fin, true);
        assert_eq!(&buf[..len], b"somethingello");
        assert_eq!(recv.len, 13);
        assert_eq!(recv.off, 13);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn overlapping_end_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"hello", 0, false);
        let second = RangeBuf::from(b"something", 3, true);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 12);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 12);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 2);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 12);
        assert_eq!(fin, true);
        assert_eq!(&buf[..len], b"helsomething");
        assert_eq!(recv.len, 12);
        assert_eq!(recv.off, 12);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn partially_multi_overlapping_reordered_read() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"hello", 8, false);
        let second = RangeBuf::from(b"something", 0, false);
        let third = RangeBuf::from(b"moar", 11, true);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 13);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 13);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 2);

        assert!(recv.write(third).is_ok());
        assert_eq!(recv.len, 15);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 3);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 15);
        assert_eq!(fin, true);
        assert_eq!(&buf[..len], b"somethinhelloar");
        assert_eq!(recv.len, 15);
        assert_eq!(recv.off, 15);
        assert_eq!(recv.data.len(), 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn partially_multi_overlapping_reordered_read2() {
        let mut recv = RecvBuf::new(std::u64::MAX);
        assert_eq!(recv.len, 0);

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"aaa", 0, false);
        let second = RangeBuf::from(b"bbb", 2, false);
        let third = RangeBuf::from(b"ccc", 4, false);
        let fourth = RangeBuf::from(b"ddd", 6, false);
        let fifth = RangeBuf::from(b"eee", 9, false);
        let sixth = RangeBuf::from(b"fff", 11, false);

        assert!(recv.write(second).is_ok());
        assert_eq!(recv.len, 5);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 1);

        assert!(recv.write(fourth).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 2);

        assert!(recv.write(third).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 3);

        assert!(recv.write(first).is_ok());
        assert_eq!(recv.len, 9);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 4);

        assert!(recv.write(sixth).is_ok());
        assert_eq!(recv.len, 14);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 5);

        assert!(recv.write(fifth).is_ok());
        assert_eq!(recv.len, 14);
        assert_eq!(recv.off, 0);
        assert_eq!(recv.data.len(), 6);

        let (len, fin) = recv.emit(&mut buf).unwrap();
        assert_eq!(len, 14);
        assert_eq!(fin, false);
        assert_eq!(&buf[..len], b"aabbbcdddeefff");
        assert_eq!(recv.len, 14);
        assert_eq!(recv.off, 14);
        assert_eq!(recv.data.len(), 0);

        assert_eq!(recv.emit(&mut buf), Err(Error::Done));
    }

    #[test]
    fn empty_write() {
        let mut buf = [0; 5];

        let mut send = SendBuf::new(std::u64::MAX);
        assert_eq!(send.len, 0);

        let (written, fin) = send.emit(&mut buf).unwrap();
        assert_eq!(written, 0);
        assert_eq!(fin, false);
    }

    #[test]
    fn multi_write() {
        let mut buf = [0; 128];

        let mut send = SendBuf::new(std::u64::MAX);
        assert_eq!(send.len, 0);

        let first = b"something";
        let second = b"helloworld";

        assert!(send.push_slice(first, false).is_ok());
        assert_eq!(send.len, 9);

        assert!(send.push_slice(second, true).is_ok());
        assert_eq!(send.len, 19);

        let (written, fin) = send.emit(&mut buf[..128]).unwrap();
        assert_eq!(written, 19);
        assert_eq!(fin, true);
        assert_eq!(&buf[..written], b"somethinghelloworld");
        assert_eq!(send.len, 0);
    }

    #[test]
    fn split_write() {
        let mut buf = [0; 10];

        let mut send = SendBuf::new(std::u64::MAX);
        assert_eq!(send.len, 0);

        let first = b"something";
        let second = b"helloworld";

        assert!(send.push_slice(first, false).is_ok());
        assert_eq!(send.len, 9);

        assert!(send.push_slice(second, true).is_ok());
        assert_eq!(send.len, 19);

        assert_eq!(send.off_front(), 0);

        let (written, fin) = send.emit(&mut buf[..10]).unwrap();
        assert_eq!(written, 10);
        assert_eq!(fin, false);
        assert_eq!(&buf[..written], b"somethingh");
        assert_eq!(send.off_front(), 10);
        assert_eq!(send.len, 9);

        let (written, fin) = send.emit(&mut buf[..5]).unwrap();
        assert_eq!(written, 5);
        assert_eq!(fin, false);
        assert_eq!(&buf[..written], b"ellow");
        assert_eq!(send.off_front(), 15);
        assert_eq!(send.len, 4);

        let (written, fin) = send.emit(&mut buf[..10]).unwrap();
        assert_eq!(written, 4);
        assert_eq!(fin, true);
        assert_eq!(&buf[..written], b"orld");
        assert_eq!(send.off_front(), 19);
        assert_eq!(send.len, 0);
    }

    #[test]
    fn resend() {
        let mut buf = [0; 15];

        let mut send = SendBuf::new(std::u64::MAX);
        assert_eq!(send.len, 0);
        assert_eq!(send.off_front(), 0);

        let first = b"something";
        let second = b"helloworld";

        assert!(send.push_slice(first, false).is_ok());
        assert_eq!(send.off_front(), 0);

        assert!(send.push_slice(second, true).is_ok());
        assert_eq!(send.off_front(), 0);

        assert_eq!(send.len, 19);

        let (written, fin) = send.emit(&mut buf[..4]).unwrap();
        assert_eq!(written, 4);
        assert_eq!(fin, false);
        assert_eq!(&buf[..written], b"some");
        assert_eq!(send.len, 15);
        assert_eq!(send.off_front(), 4);

        let (written, fin) = send.emit(&mut buf[..5]).unwrap();
        assert_eq!(written, 5);
        assert_eq!(fin, false);
        assert_eq!(&buf[..written], b"thing");
        assert_eq!(send.len, 10);
        assert_eq!(send.off_front(), 9);

        let (written, fin) = send.emit(&mut buf[..5]).unwrap();
        assert_eq!(written, 5);
        assert_eq!(fin, false);
        assert_eq!(&buf[..written], b"hello");
        assert_eq!(send.len, 5);
        assert_eq!(send.off_front(), 14);

        send.retransmit(4, 5);
        assert_eq!(send.len, 10);
        assert_eq!(send.off_front(), 4);

        send.retransmit(0, 4);
        assert_eq!(send.len, 14);
        assert_eq!(send.off_front(), 0);

        let (written, fin) = send.emit(&mut buf[..11]).unwrap();
        assert_eq!(written, 9);
        assert_eq!(fin, false);
        assert_eq!(&buf[..written], b"something");
        assert_eq!(send.len, 5);
        assert_eq!(send.off_front(), 14);

        let (written, fin) = send.emit(&mut buf[..11]).unwrap();
        assert_eq!(written, 5);
        assert_eq!(fin, false);
        assert_eq!(&buf[..written], b"world");
        assert_eq!(send.len, 0);
        assert_eq!(send.off_front(), 19);
    }

    #[test]
    fn write_blocked_by_off() {
        let mut send = SendBuf::default();
        assert_eq!(send.len, 0);

        let first = b"something";
        let second = b"helloworld";

        assert_eq!(send.push_slice(first, false), Ok(0));
        assert_eq!(send.len, 0);

        assert_eq!(send.push_slice(second, true), Ok(0));
        assert_eq!(send.len, 0);

        send.update_max_data(5);

        assert_eq!(send.push_slice(first, false), Ok(5));
        assert_eq!(send.len, 5);

        assert_eq!(send.push_slice(second, true), Ok(0));
        assert_eq!(send.len, 5);

        let write = send.pop(10).unwrap();
        assert_eq!(write.off(), 0);
        assert_eq!(write.len(), 5);
        assert_eq!(write.fin(), false);
        assert_eq!(&write[..], b"somet");
        assert_eq!(send.len, 0);

        let write = send.pop(10).unwrap();
        assert_eq!(write.off(), 5);
        assert_eq!(write.len(), 0);
        assert_eq!(write.fin(), false);
        assert_eq!(&write[..], b"");
        assert_eq!(send.len, 0);

        send.update_max_data(15);

        assert_eq!(send.push_slice(&first[5..], false), Ok(4));
        assert_eq!(send.len, 4);

        assert_eq!(send.push_slice(second, true), Ok(6));
        assert_eq!(send.len, 10);

        let write = send.pop(10).unwrap();
        assert_eq!(write.off(), 5);
        assert_eq!(write.len(), 10);
        assert_eq!(write.fin(), false);
        assert_eq!(&write[..], b"hinghellow");
        assert_eq!(send.len, 0);

        send.update_max_data(25);

        assert_eq!(send.push_slice(&second[6..], true), Ok(4));
        assert_eq!(send.len, 4);

        let write = send.pop(10).unwrap();
        assert_eq!(write.off(), 15);
        assert_eq!(write.len(), 4);
        assert_eq!(write.fin(), true);
        assert_eq!(&write[..], b"orld");
        assert_eq!(send.len, 0);
    }

    #[test]
    fn zero_len_write() {
        let mut send = SendBuf::new(std::u64::MAX);
        assert_eq!(send.len, 0);

        let first = b"something";

        assert!(send.push_slice(first, false).is_ok());
        assert_eq!(send.len, 9);

        assert!(send.push_slice(&[], true).is_ok());
        assert_eq!(send.len, 9);

        let write = send.pop(10).unwrap();
        assert_eq!(write.off(), 0);
        assert_eq!(write.len(), 9);
        assert_eq!(write.fin(), true);
        assert_eq!(&write[..], b"something");
        assert_eq!(send.len, 0);
    }

    #[test]
    fn recv_flow_control() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"hello", 0, false);
        let second = RangeBuf::from(b"world", 5, false);
        let third = RangeBuf::from(b"something", 10, false);

        assert_eq!(stream.recv.write(second), Ok(()));
        assert_eq!(stream.recv.write(first), Ok(()));
        assert!(!stream.recv.almost_full());

        assert_eq!(stream.recv.write(third), Err(Error::FlowControl));

        let (len, fin) = stream.recv.emit(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"helloworld");
        assert_eq!(fin, false);

        assert!(stream.recv.almost_full());

        stream.recv.update_max_data();
        assert_eq!(stream.recv.max_data_next(), 25);
        assert!(!stream.recv.almost_full());

        let third = RangeBuf::from(b"something", 10, false);
        assert_eq!(stream.recv.write(third), Ok(()));
    }

    #[test]
    fn recv_past_fin() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let first = RangeBuf::from(b"hello", 0, true);
        let second = RangeBuf::from(b"world", 5, false);

        assert_eq!(stream.recv.write(first), Ok(()));
        assert_eq!(stream.recv.write(second), Err(Error::FinalSize));
    }

    #[test]
    fn recv_fin_dup() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let first = RangeBuf::from(b"hello", 0, true);
        let second = RangeBuf::from(b"hello", 0, true);

        assert_eq!(stream.recv.write(first), Ok(()));
        assert_eq!(stream.recv.write(second), Ok(()));

        let mut buf = [0; 32];

        let (len, fin) = stream.recv.emit(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"hello");
        assert_eq!(fin, true);
    }

    #[test]
    fn recv_fin_change() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let first = RangeBuf::from(b"hello", 0, true);
        let second = RangeBuf::from(b"world", 5, true);

        assert_eq!(stream.recv.write(second), Ok(()));
        assert_eq!(stream.recv.write(first), Err(Error::FinalSize));
    }

    #[test]
    fn recv_fin_lower_than_received() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let first = RangeBuf::from(b"hello", 0, true);
        let second = RangeBuf::from(b"world", 5, false);

        assert_eq!(stream.recv.write(second), Ok(()));
        assert_eq!(stream.recv.write(first), Err(Error::FinalSize));
    }

    #[test]
    fn recv_fin_flow_control() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let mut buf = [0; 32];

        let first = RangeBuf::from(b"hello", 0, false);
        let second = RangeBuf::from(b"world", 5, true);

        assert_eq!(stream.recv.write(first), Ok(()));
        assert_eq!(stream.recv.write(second), Ok(()));

        let (len, fin) = stream.recv.emit(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"helloworld");
        assert_eq!(fin, true);

        assert!(!stream.recv.almost_full());
    }

    #[test]
    fn recv_fin_reset_mismatch() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let first = RangeBuf::from(b"hello", 0, true);

        assert_eq!(stream.recv.write(first), Ok(()));
        assert_eq!(stream.recv.reset(10), Err(Error::FinalSize));
    }

    #[test]
    fn recv_reset_dup() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let first = RangeBuf::from(b"hello", 0, false);

        assert_eq!(stream.recv.write(first), Ok(()));
        assert_eq!(stream.recv.reset(5), Ok(0));
        assert_eq!(stream.recv.reset(5), Ok(0));
    }

    #[test]
    fn recv_reset_change() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let first = RangeBuf::from(b"hello", 0, false);

        assert_eq!(stream.recv.write(first), Ok(()));
        assert_eq!(stream.recv.reset(5), Ok(0));
        assert_eq!(stream.recv.reset(10), Err(Error::FinalSize));
    }

    #[test]
    fn recv_reset_lower_than_received() {
        let mut stream = Stream::new(15, 0, true, true);
        assert!(!stream.recv.almost_full());

        let first = RangeBuf::from(b"hello", 0, false);

        assert_eq!(stream.recv.write(first), Ok(()));
        assert_eq!(stream.recv.reset(4), Err(Error::FinalSize));
    }

    #[test]
    fn send_flow_control() {
        let mut stream = Stream::new(0, 15, true, true);

        let first = b"hello";
        let second = b"world";
        let third = b"something";

        assert!(stream.send.push_slice(first, false).is_ok());
        assert!(stream.send.push_slice(second, false).is_ok());
        assert!(stream.send.push_slice(third, false).is_ok());

        let write = stream.send.pop(25).unwrap();
        assert_eq!(write.off(), 0);
        assert_eq!(write.len(), 15);
        assert_eq!(write.fin(), false);
        assert_eq!(write.data, b"helloworldsomet");

        let write = stream.send.pop(25).unwrap();
        assert_eq!(write.off(), 15);
        assert_eq!(write.len(), 0);
        assert_eq!(write.fin(), false);
        assert_eq!(write.data, b"");

        let first = RangeBuf::from(b"helloworldsomet", 0, false);
        assert_eq!(stream.send.push(first), Ok(()));

        let write = stream.send.pop(10).unwrap();
        assert_eq!(write.off(), 0);
        assert_eq!(write.len(), 10);
        assert_eq!(write.fin(), false);
        assert_eq!(write.data, b"helloworld");

        let write = stream.send.pop(10).unwrap();
        assert_eq!(write.off(), 10);
        assert_eq!(write.len(), 5);
        assert_eq!(write.fin(), false);
        assert_eq!(write.data, b"somet");
    }

    #[test]
    fn send_past_fin() {
        let mut stream = Stream::new(0, 15, true, true);

        let first = b"hello";
        let second = b"world";
        let third = b"third";

        assert_eq!(stream.send.push_slice(first, false), Ok(5));

        assert_eq!(stream.send.push_slice(second, true), Ok(5));
        assert!(stream.send.is_fin());

        assert_eq!(stream.send.push_slice(third, false), Err(Error::FinalSize));
    }

    #[test]
    fn send_fin_dup() {
        let mut stream = Stream::new(0, 15, true, true);

        let first = RangeBuf::from(b"hello", 0, true);
        let second = RangeBuf::from(b"hello", 0, true);

        assert_eq!(stream.send.push(first), Ok(()));
        assert_eq!(stream.send.push(second), Ok(()));
    }

    #[test]
    fn send_undo_fin() {
        let mut stream = Stream::new(0, 15, true, true);

        let first = b"hello";
        let second = RangeBuf::from(b"hello", 0, false);

        assert_eq!(stream.send.push_slice(first, true), Ok(5));
        assert!(stream.send.is_fin());

        assert_eq!(stream.send.push(second), Err(Error::FinalSize));
    }

    #[test]
    fn send_fin_max_data_match() {
        let mut stream = Stream::new(0, 15, true, true);

        let slice = b"hellohellohello";

        assert!(stream.send.push_slice(slice, true).is_ok());

        let write = stream.send.pop(15).unwrap();
        assert_eq!(write.off(), 0);
        assert_eq!(write.len(), 15);
        assert_eq!(write.fin(), true);
        assert_eq!(write.data, slice);
    }

    #[test]
    fn send_fin_zero_length() {
        let mut stream = Stream::new(0, 15, true, true);

        assert_eq!(stream.send.push_slice(b"hello", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"", true), Ok(0));
        assert!(stream.send.is_fin());

        let write = stream.send.pop(5).unwrap();
        assert_eq!(write.off(), 0);
        assert_eq!(write.len(), 5);
        assert_eq!(write.fin(), true);
        assert_eq!(write.data, b"hello");
    }

    #[test]
    fn send_ack() {
        let mut stream = Stream::new(0, 15, true, true);

        assert_eq!(stream.send.push_slice(b"hello", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"world", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"", true), Ok(0));
        assert!(stream.send.is_fin());

        let write = stream.send.pop(5).unwrap();
        assert_eq!(write.off(), 0);
        assert_eq!(write.len(), 5);
        assert_eq!(write.fin(), false);
        assert_eq!(write.data, b"hello");

        stream.send.ack(write.off(), write.len());

        assert_eq!(stream.send.push(write), Ok(()));

        let write = stream.send.pop(5).unwrap();
        assert_eq!(write.off(), 5);
        assert_eq!(write.len(), 5);
        assert_eq!(write.fin(), true);
        assert_eq!(write.data, b"world");
    }

    #[test]
    fn send_ack_reordering() {
        let mut stream = Stream::new(0, 15, true, true);

        assert_eq!(stream.send.push_slice(b"hello", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"world", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"", true), Ok(0));
        assert!(stream.send.is_fin());

        let write1 = stream.send.pop(5).unwrap();
        assert_eq!(write1.off(), 0);
        assert_eq!(write1.len(), 5);
        assert_eq!(write1.fin(), false);
        assert_eq!(write1.data, b"hello");

        let write2 = stream.send.pop(1).unwrap();
        assert_eq!(write2.off(), 5);
        assert_eq!(write2.len(), 1);
        assert_eq!(write2.fin(), false);
        assert_eq!(write2.data, b"w");

        stream.send.ack(write2.off(), write2.len());
        stream.send.ack(write1.off(), write1.len());

        assert_eq!(stream.send.push(write1), Ok(()));
        assert_eq!(stream.send.push(write2), Ok(()));

        let write = stream.send.pop(5).unwrap();
        assert_eq!(write.off(), 6);
        assert_eq!(write.len(), 4);
        assert_eq!(write.fin(), true);
        assert_eq!(write.data, b"orld");
    }

    #[test]
    fn recv_data_below_off() {
        let mut stream = Stream::new(15, 0, true, true);

        let first = RangeBuf::from(b"hello", 0, false);

        assert_eq!(stream.recv.write(first), Ok(()));

        let mut buf = [0; 10];

        let (len, fin) = stream.recv.emit(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"hello");
        assert_eq!(fin, false);

        let first = RangeBuf::from(b"elloworld", 1, true);
        assert_eq!(stream.recv.write(first), Ok(()));

        let (len, fin) = stream.recv.emit(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"world");
        assert_eq!(fin, true);
    }

    #[test]
    fn stream_complete() {
        let mut stream = Stream::new(30, 30, true, true);

        assert_eq!(stream.send.push_slice(b"hello", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"world", false), Ok(5));

        assert!(!stream.send.is_complete());
        assert!(!stream.send.is_fin());

        assert_eq!(stream.send.push_slice(b"", true), Ok(0));

        assert!(!stream.send.is_complete());
        assert!(stream.send.is_fin());

        let buf = RangeBuf::from(b"hello", 0, true);
        assert!(stream.recv.write(buf).is_ok());
        assert!(!stream.recv.is_fin());

        stream.send.ack(6, 4);
        assert!(!stream.send.is_complete());

        let mut buf = [0; 2];
        assert_eq!(stream.recv.emit(&mut buf), Ok((2, false)));
        assert!(!stream.recv.is_fin());

        stream.send.ack(1, 5);
        assert!(!stream.send.is_complete());

        stream.send.ack(0, 1);
        assert!(stream.send.is_complete());

        assert!(!stream.is_complete());

        let mut buf = [0; 3];
        assert_eq!(stream.recv.emit(&mut buf), Ok((3, true)));
        assert!(stream.recv.is_fin());

        assert!(stream.is_complete());
    }

    #[test]
    fn send_fin_zero_length_output() {
        let mut buf = [0; 5];

        let mut stream = Stream::new(0, 15, true, true);

        assert_eq!(stream.send.push_slice(b"hello", false), Ok(5));
        assert_eq!(stream.send.off_front(), 0);
        assert!(!stream.send.is_fin());

        let (written, fin) = stream.send.emit(&mut buf).unwrap();
        assert_eq!(written, 5);
        assert_eq!(fin, false);
        assert_eq!(&buf[..written], b"hello");

        assert_eq!(stream.send.push_slice(b"", true), Ok(0));
        assert!(stream.send.is_fin());
        assert_eq!(stream.send.off_front(), 5);

        let (written, fin) = stream.send.emit(&mut buf).unwrap();
        assert_eq!(written, 0);
        assert_eq!(fin, true);
        assert_eq!(&buf[..written], b"");
    }

    #[test]
    fn send_emit() {
        let mut buf = [0; 5];

        let mut stream = Stream::new(0, 20, true, true);

        assert_eq!(stream.send.push_slice(b"hello", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"world", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"olleh", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"dlrow", true), Ok(5));
        assert_eq!(stream.send.off_front(), 0);
        assert_eq!(stream.send.data.len(), 4);

        assert!(stream.is_flushable());

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..4]), Ok((4, false)));
        assert_eq!(stream.send.off_front(), 4);
        assert_eq!(&buf[..4], b"hell");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..4]), Ok((4, false)));
        assert_eq!(stream.send.off_front(), 8);
        assert_eq!(&buf[..4], b"owor");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..2]), Ok((2, false)));
        assert_eq!(stream.send.off_front(), 10);
        assert_eq!(&buf[..2], b"ld");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..1]), Ok((1, false)));
        assert_eq!(stream.send.off_front(), 11);
        assert_eq!(&buf[..1], b"o");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((5, false)));
        assert_eq!(stream.send.off_front(), 16);
        assert_eq!(&buf[..5], b"llehd");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((4, true)));
        assert_eq!(stream.send.off_front(), 20);
        assert_eq!(&buf[..4], b"lrow");

        assert!(!stream.is_flushable());

        assert!(!stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((0, true)));
        assert_eq!(stream.send.off_front(), 20);
    }

    #[test]
    fn send_emit_ack() {
        let mut buf = [0; 5];

        let mut stream = Stream::new(0, 20, true, true);

        assert_eq!(stream.send.push_slice(b"hello", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"world", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"olleh", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"dlrow", true), Ok(5));
        assert_eq!(stream.send.off_front(), 0);
        assert_eq!(stream.send.data.len(), 4);

        assert!(stream.is_flushable());

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..4]), Ok((4, false)));
        assert_eq!(stream.send.off_front(), 4);
        assert_eq!(&buf[..4], b"hell");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..4]), Ok((4, false)));
        assert_eq!(stream.send.off_front(), 8);
        assert_eq!(&buf[..4], b"owor");

        stream.send.ack_and_drop(0, 5);
        assert_eq!(stream.send.data.len(), 3);

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..2]), Ok((2, false)));
        assert_eq!(stream.send.off_front(), 10);
        assert_eq!(&buf[..2], b"ld");

        stream.send.ack_and_drop(7, 5);
        assert_eq!(stream.send.data.len(), 3);

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..1]), Ok((1, false)));
        assert_eq!(stream.send.off_front(), 11);
        assert_eq!(&buf[..1], b"o");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((5, false)));
        assert_eq!(stream.send.off_front(), 16);
        assert_eq!(&buf[..5], b"llehd");

        stream.send.ack_and_drop(5, 7);
        assert_eq!(stream.send.data.len(), 2);

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((4, true)));
        assert_eq!(stream.send.off_front(), 20);
        assert_eq!(&buf[..4], b"lrow");

        assert!(!stream.is_flushable());

        assert!(!stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((0, true)));
        assert_eq!(stream.send.off_front(), 20);

        stream.send.ack_and_drop(22, 4);
        assert_eq!(stream.send.data.len(), 2);

        stream.send.ack_and_drop(20, 1);
        assert_eq!(stream.send.data.len(), 2);
    }

    #[test]
    fn send_emit_retransmit() {
        let mut buf = [0; 5];

        let mut stream = Stream::new(0, 20, true, true);

        assert_eq!(stream.send.push_slice(b"hello", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"world", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"olleh", false), Ok(5));
        assert_eq!(stream.send.push_slice(b"dlrow", true), Ok(5));
        assert_eq!(stream.send.off_front(), 0);
        assert_eq!(stream.send.data.len(), 4);

        assert!(stream.is_flushable());

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..4]), Ok((4, false)));
        assert_eq!(stream.send.off_front(), 4);
        assert_eq!(&buf[..4], b"hell");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..4]), Ok((4, false)));
        assert_eq!(stream.send.off_front(), 8);
        assert_eq!(&buf[..4], b"owor");

        stream.send.retransmit(3, 3);
        assert_eq!(stream.send.off_front(), 3);

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..3]), Ok((3, false)));
        assert_eq!(stream.send.off_front(), 6);
        assert_eq!(&buf[..3], b"low");

        // TODO: fix spurious retransmission
        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..2]), Ok((2, false)));
        assert_eq!(stream.send.off_front(), 8);
        assert_eq!(&buf[..2], b"or");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..2]), Ok((2, false)));
        assert_eq!(stream.send.off_front(), 10);
        assert_eq!(&buf[..2], b"ld");

        stream.send.ack_and_drop(7, 2);

        stream.send.retransmit(8, 2);

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..2]), Ok((2, false)));
        assert_eq!(stream.send.off_front(), 10);
        assert_eq!(&buf[..2], b"ld");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..1]), Ok((1, false)));
        assert_eq!(stream.send.off_front(), 11);
        assert_eq!(&buf[..1], b"o");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((5, false)));
        assert_eq!(stream.send.off_front(), 16);
        assert_eq!(&buf[..5], b"llehd");

        stream.send.retransmit(12, 2);

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..2]), Ok((2, false)));
        assert_eq!(stream.send.off_front(), 14);
        assert_eq!(&buf[..2], b"le");

        // TODO: fix spurious retransmission
        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..2]), Ok((1, false)));
        assert_eq!(stream.send.off_front(), 16);
        assert_eq!(&buf[..1], b"h");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((4, true)));
        assert_eq!(stream.send.off_front(), 20);
        assert_eq!(&buf[..4], b"lrow");

        assert!(!stream.is_flushable());

        assert!(!stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((0, true)));
        assert_eq!(stream.send.off_front(), 20);

        stream.send.retransmit(7, 12);

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((5, false)));
        assert_eq!(stream.send.off_front(), 12);
        assert_eq!(&buf[..5], b"rldol");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((5, false)));
        assert_eq!(stream.send.off_front(), 17);
        assert_eq!(&buf[..5], b"lehdl");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((3, true)));
        assert_eq!(stream.send.off_front(), 20);
        assert_eq!(&buf[..3], b"row");

        stream.send.ack_and_drop(12, 7);

        stream.send.retransmit(7, 12);

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((5, false)));
        assert_eq!(stream.send.off_front(), 12);
        assert_eq!(&buf[..5], b"rldol");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((5, false)));
        assert_eq!(stream.send.off_front(), 17);
        assert_eq!(&buf[..5], b"lehdl");

        assert!(stream.send.ready());
        assert_eq!(stream.send.emit(&mut buf[..5]), Ok((3, true)));
        assert_eq!(stream.send.off_front(), 20);
        assert_eq!(&buf[..3], b"row");

        // stream.send.ack_and_drop(20, 1);
        // assert_eq!(stream.send.data.len(), 2);
    }
}
