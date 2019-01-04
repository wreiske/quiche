// Copyright (C) 2018, Cloudflare, Inc.
// Copyright (C) 2018, Alessandro Ghedini
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright
//       notice, this list of conditions and the following disclaimer.
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

use crate::Result;
use crate::Error;

use crate::crypto;
use crate::octets;
use crate::rand;
use crate::ranges;
use crate::recovery;
use crate::stream;

const FORM_BIT: u8 = 0x80;
const FIXED_BIT: u8 = 0x40;
const KEY_PHASE_BIT: u8 = 0x04;

const TYPE_MASK: u8 = 0x30;
const PKT_NUM_MASK: u8 = 0x03;

const MAX_CID_LEN: u8 = 18;

/// QUIC packet type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Type {
    Initial,
    Retry,
    Handshake,
    ZeroRTT,
    VersionNegotiation,
    Application,
}

/// A QUIC packet's header.
#[derive(Clone, PartialEq)]
pub struct Header {
    /// The type of the packet.
    pub ty: Type,

    /// The version of the packet.
    pub version: u32,

    /// The destination connection ID of the packet.
    pub dcid: Vec<u8>,

    /// The source connection ID of the packet.
    pub scid: Vec<u8>,

    /// The length of the packet number.
    pub pkt_num_len: u8,

    /// The address verification token of the packet. Only present in `Initial`
    /// packets.
    pub token: Option<Vec<u8>>,

    /// The list of versions in the packet. Only present in `VersionNegotiation`
    /// packets.
    pub versions: Option<Vec<u32>>,
}

impl Header {
    /// Parses a QUIC packet header from the given buffer.
    ///
    /// The `dcil` parameter is the length of the destionation connection ID,
    /// required to parse short header packets.
    pub fn from_slice(buf: &mut [u8], dcil: usize) -> Result<Header> {
        let mut b = octets::Bytes::new(buf);
        Header::from_bytes(&mut b, dcil)
    }

    pub(crate) fn from_bytes(b: &mut octets::Bytes, dcil: usize) -> Result<Header> {
        let first = b.get_u8()?;

        if !Header::is_long(first) {
            // Decode short header.
            let dcid = b.get_bytes(dcil)?;

            return Ok(Header {
                ty: Type::Application,
                version: 0,
                dcid: dcid.to_vec(),
                scid: Vec::new(),
                pkt_num_len: 0,
                token: None,
                versions: None,
            });
        }

        // Decode long header.
        let version = b.get_u32()?;

        let ty = if version == 0 {
            Type::VersionNegotiation
        } else {
            match (first & TYPE_MASK) >> 4 {
                0x00 => Type::Initial,
                0x01 => Type::ZeroRTT,
                0x02 => Type::Handshake,
                0x03 => Type::Retry,
                _    => return Err(Error::InvalidPacket),
            }
        };

        let (dcil, scil) = match b.get_u8() {
            Ok(v) => {
                let mut dcil = v >> 4;
                let mut scil = v & 0xf;

                if dcil > MAX_CID_LEN || scil > MAX_CID_LEN {
                    return Err(Error::InvalidPacket);
                }

                if dcil > 0 {
                    dcil += 3;
                }

                if scil > 0 {
                    scil += 3;
                }

                (dcil, scil)
            },

            Err(_) => return Err(Error::BufferTooShort),
        };

        let dcid = b.get_bytes(dcil as usize)?.to_vec();
        let scid = b.get_bytes(scil as usize)?.to_vec();

        // End of invariants.

        let mut token: Option<Vec<u8>> = None;
        let mut versions: Option<Vec<u32>> = None;

        match ty {
            Type::Initial => {
                // Only Initial packet have a token.
                token = Some(b.get_bytes_with_varint_length()?.to_vec());
            },

            Type::Retry => {
                panic!("Retry not supported");
            },

            Type::VersionNegotiation => {
                let mut list: Vec<u32> = Vec::new();

                while b.cap() > 0 {
                    let version = b.get_u32()?;
                    list.push(version);
                }

                versions = Some(list);
            },

            _ => (),
        };

        Ok(Header {
            ty,
            version,
            dcid,
            scid,
            pkt_num_len: 0,
            token,
            versions,
        })
    }

    pub(crate) fn to_bytes(&self, out: &mut octets::Bytes) -> Result<()> {
        let mut first = 0;

        // Encode pkt num length.
        first |= self.pkt_num_len.checked_sub(1)
                                 .unwrap_or(0);

        // Encode short header.
        if self.ty == Type::Application {
            // Unset form bit for short header.
            first &= !FORM_BIT;

            // Set fixed bit.
            first |= FIXED_BIT;

            // TODO: support key update
            first &= !KEY_PHASE_BIT;

            out.put_u8(first)?;
            out.put_bytes(&self.dcid)?;

            return Ok(());
        }

        // Encode long header.
        let ty: u8 = match self.ty {
                Type::Initial   => 0x00,
                Type::ZeroRTT   => 0x01,
                Type::Handshake => 0x02,
                Type::Retry     => 0x03,
                _               => return Err(Error::InvalidPacket),
        };

        first |= FORM_BIT | FIXED_BIT | (ty << 4);

        out.put_u8(first)?;

        out.put_u32(self.version)?;

        let mut cil: u8 = 0;

        if !self.dcid.is_empty() {
            cil |= ((self.dcid.len() - 3) as u8) << 4;
        }

        if !self.scid.is_empty() {
            cil |= ((self.scid.len() - 3) as u8) & 0xf;
        }

        out.put_u8(cil)?;

        out.put_bytes(&self.dcid)?;
        out.put_bytes(&self.scid)?;

        // Only Initial packet have a token.
        if self.ty == Type::Initial {
            match self.token {
                Some(ref v) => {
                    out.put_bytes(v)?;
                },

                None => {
                    // No token, so lemgth = 0.
                    out.put_varint(0)?;
                }
            }
        }

        Ok(())
    }

    /// Returns true if the packet has a long header.
    ///
    /// The `b` parameter represents the first byte of the QUIC header.
    pub fn is_long(b: u8) -> bool {
        b & FORM_BIT != 0
    }
}

impl std::fmt::Debug for Header {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.ty)?;

        if self.ty != Type::Application {
            write!(f, " version={:x}", self.version)?;
        }

        write!(f, " dcid=")?;
        for b in &self.dcid {
            write!(f, "{:02x}", b)?;
        }

        if self.ty != Type::Application {
            write!(f, " scid=")?;
            for b in &self.scid {
                write!(f, "{:02x}", b)?;
            }
        }

        if self.ty == Type::VersionNegotiation {
            if let Some(ref versions) = self.versions {
                write!(f, " versions={:x?}", versions)?;
            }
        }

        Ok(())
    }
}

pub fn pkt_num_len(pn: u64) -> Result<usize> {
    let len = if pn < std::u8::MAX as u64 {
        1
    } else if pn < std::u16::MAX as u64 {
        2
    } else if pn < std::u32::MAX as u64 {
        4
    } else {
        return Err(Error::InvalidPacket);
    };

    Ok(len)
}

pub fn pkt_num_bits(len: usize) -> Result<usize> {
    let bits = match len {
        1 => 7,
        2 => 14,
        4 => 30,
        _ => return Err(Error::InvalidPacket),
    };

    Ok(bits)
}

pub fn decrypt_hdr(b: &mut octets::Bytes, aead: &crypto::Open)
                                                    -> Result<(u64, usize)> {
    let mut first = {
        let (first_buf, _) = b.split_at(1)?;
        first_buf.as_ref()[0]
    };

    let mut pn_and_sample = b.peek_bytes(4 + aead.alg().pn_nonce_len())?;

    let (mut ciphertext, sample) = pn_and_sample.split_at(4).unwrap();

    let ciphertext = ciphertext.as_mut();

    let mask = aead.new_mask(sample.as_ref())?;

    if Header::is_long(first) {
        first ^= mask[0] & 0x0f;
    } else {
        first ^= mask[0] & 0x1f;
    }

    let pn_len = usize::from((first & PKT_NUM_MASK) + 1);

    let ciphertext = &mut ciphertext[..pn_len];

    for i in 0..pn_len {
        ciphertext[i] ^= mask[i + 1];
    }

    // Extract packet number corresponding to the decoded length.
    let pn = match pn_len {
        1 => u64::from(b.get_u8()?),

        2 => u64::from(b.get_u16()?),

        3 => u64::from(b.get_u24()?),

        4 => u64::from(b.get_u32()?),

        _ => return Err(Error::InvalidPacket),
    };

    let (mut first_buf, _) = b.split_at(1)?;
    first_buf.as_mut()[0] = first;

    Ok((pn, pn_len))
}

pub fn decode_pkt_num(largest_pn: u64, truncated_pn: u64, pn_len: usize) -> Result<u64> {
    let pn_nbits     = pkt_num_bits(pn_len)?;
    let expected_pn  = largest_pn + 1;
    let pn_win       = 1 << pn_nbits;
    let pn_hwin      = pn_win / 2;
    let pn_mask      = pn_win - 1;
    let candidate_pn = (expected_pn & !pn_mask) | truncated_pn;

    if candidate_pn + pn_hwin <= expected_pn {
         return Ok(candidate_pn + pn_win);
    }

    if candidate_pn > expected_pn + pn_hwin && candidate_pn > pn_win {
        return Ok(candidate_pn - pn_win);
    }

    Ok(candidate_pn)
}

pub fn encrypt_hdr(b: &mut octets::Bytes, pn_len: usize, payload: &[u8],
                   aead: &crypto::Seal) -> Result<()> {
    let sample = &payload[4 - pn_len..16 + (4 - pn_len)];

    let mask = aead.new_mask(sample)?;

    let (mut first, mut rest) = b.split_at(1)?;

    let first = first.as_mut();

    if Header::is_long(first[0]) {
        first[0] ^= mask[0] & 0x0f;
    } else {
        first[0] ^= mask[0] & 0x1f;
    }

    let pn_buf = rest.slice_last(pn_len)?;
    for i in 0..pn_len {
        pn_buf[i] ^= mask[i + 1];
    }

    Ok(())
}

pub fn encode_pkt_num(pn: u64, b: &mut octets::Bytes) -> Result<()> {
    let len = pkt_num_len(pn)?;

    match len {
        1 => b.put_u8(pn as u8)?,

        2 => b.put_u16(pn as u16)?,

        3 => b.put_u24(pn as u32)?,

        4 => b.put_u32(pn as u32)?,

        _ => return Err(Error::InvalidPacket),
    };

    Ok(())
}

pub fn negotiate_version(hdr: &Header, out: &mut [u8]) -> Result<usize> {
    let mut b = octets::Bytes::new(out);

    let first = rand::rand_u8() | FORM_BIT;

    b.put_u8(first)?;
    b.put_u32(0)?;

    // Invert client's scid and dcid.
    let mut cil: u8 = 0;
    if !hdr.scid.is_empty() {
        cil |= ((hdr.scid.len() - 3) as u8) << 4;
    }

    if !hdr.dcid.is_empty() {
        cil |= ((hdr.dcid.len() - 3) as u8) & 0xf;
    }

    b.put_u8(cil)?;
    b.put_bytes(&hdr.scid)?;
    b.put_bytes(&hdr.dcid)?;
    b.put_u32(crate::VERSION_DRAFT17)?;

    Ok(b.off())
}

pub struct PktNumSpace {
    pub pkt_type: Type,

    pub largest_rx_pkt_num: u64,

    pub next_pkt_num: u64,

    pub recv_pkt_need_ack: ranges::RangeSet,

    pub recv_pkt_num: PktNumWindow,

    pub flight: recovery::InFlight,

    pub do_ack: bool,

    pub crypto_level: crypto::Level,

    pub crypto_open: Option<crypto::Open>,
    pub crypto_seal: Option<crypto::Seal>,

    pub crypto_stream: stream::Stream,
}

impl PktNumSpace {
    pub fn new(ty: Type, crypto_level: crypto::Level) -> PktNumSpace {
        PktNumSpace {
            pkt_type: ty,

            largest_rx_pkt_num: 0,

            next_pkt_num: 0,

            recv_pkt_need_ack: ranges::RangeSet::default(),

            recv_pkt_num: PktNumWindow::default(),

            flight: recovery::InFlight::default(),

            do_ack: false,

            crypto_level,

            crypto_open: None,
            crypto_seal: None,

            crypto_stream: stream::Stream::new(std::usize::MAX, std::usize::MAX),
        }
    }

    pub fn clear(&mut self) {
        self.flight = recovery::InFlight::default();
        self.crypto_stream = stream::Stream::new(std::usize::MAX,
                                                 std::usize::MAX);
    }

    pub fn cipher(&self) -> crypto::Algorithm {
        match self.crypto_open {
            Some(ref v) => v.alg(),
            None => crypto::Algorithm::Null,
        }
    }

    pub fn overhead(&self) -> usize {
        self.crypto_seal.as_ref().unwrap().alg().tag_len()
    }

    pub fn ready(&self) -> bool {
        self.crypto_stream.writable() || !self.flight.lost.is_empty() || self.do_ack
    }
}

#[derive(Clone, Copy, Default)]
pub struct PktNumWindow {
    lower: u64,
    window: u128,
}

impl PktNumWindow {
    pub fn insert(&mut self, seq: u64) {
        // Packet is on the left end of the window.
        if seq < self.lower {
            return;
        }

        // Packet is on the right end of the window.
        if seq > self.upper() {
            let diff = seq - self.upper();
            self.lower += diff;

            self.window = self.window.checked_shl(diff as u32)
                                     .unwrap_or(0);
        }

        let mask = 1_u128 << (self.upper() - seq);
        self.window |= mask;
    }

    pub fn contains(&mut self, seq: u64) -> bool {
        // Packet is on the right end of the window.
        if seq > self.upper() {
            return false;
        }

        // Packet is on the left end of the window.
        if seq < self.lower {
            return true;
        }

        let mask = 1_u128 << (self.upper() - seq);
        self.window & mask != 0
    }

    fn upper(&self) -> u64 {
        self.lower.checked_add(std::mem::size_of::<u128>() as u64 * 8)
                  .unwrap_or(std::u64::MAX) - 1
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_header() {
        let hdr = Header {
            ty: Type::Handshake,
            version: 0xafafafaf,
            dcid: vec![ 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba ],
            scid: vec![ 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb ],
            pkt_num_len: 0,
            token: None,
            versions: None,
        };

        let mut d: [u8; 50] = [0; 50];

        let mut b = octets::Bytes::new(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Bytes::new(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 9).unwrap(), hdr);
    }

    #[test]
    fn short_header() {
        let hdr = Header {
            ty: Type::Application,
            version: 0,
            dcid: vec![ 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba ],
            scid: vec![ ],
            pkt_num_len: 0,
            token: None,
            versions: None,
        };

        let mut d: [u8; 50] = [0; 50];

        let mut b = octets::Bytes::new(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Bytes::new(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 9).unwrap(), hdr);
    }

    #[test]
    fn pkt_num_window() {
        let mut win = PktNumWindow::default();
        assert_eq!(win.lower, 0);
        assert!(!win.contains(0));
        assert!(!win.contains(1));

        win.insert(0);
        assert_eq!(win.lower, 0);
        assert!(win.contains(0));
        assert!(!win.contains(1));

        win.insert(1);
        assert_eq!(win.lower, 0);
        assert!(win.contains(0));
        assert!(win.contains(1));

        win.insert(3);
        assert_eq!(win.lower, 0);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(!win.contains(2));
        assert!(win.contains(3));

        win.insert(10);
        assert_eq!(win.lower, 0);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(!win.contains(2));
        assert!(win.contains(3));
        assert!(!win.contains(4));
        assert!(!win.contains(5));
        assert!(!win.contains(6));
        assert!(!win.contains(7));
        assert!(!win.contains(8));
        assert!(!win.contains(9));
        assert!(win.contains(10));

        win.insert(132);
        assert_eq!(win.lower, 5);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(win.contains(2));
        assert!(win.contains(3));
        assert!(win.contains(4));
        assert!(!win.contains(5));
        assert!(!win.contains(6));
        assert!(!win.contains(7));
        assert!(!win.contains(8));
        assert!(!win.contains(9));
        assert!(win.contains(10));
        assert!(!win.contains(128));
        assert!(!win.contains(130));
        assert!(!win.contains(131));
        assert!(win.contains(132));

        win.insert(1024);
        assert_eq!(win.lower, 897);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(win.contains(2));
        assert!(win.contains(3));
        assert!(win.contains(4));
        assert!(win.contains(5));
        assert!(win.contains(6));
        assert!(win.contains(7));
        assert!(win.contains(8));
        assert!(win.contains(9));
        assert!(win.contains(10));
        assert!(win.contains(128));
        assert!(win.contains(130));
        assert!(win.contains(132));
        assert!(win.contains(896));
        assert!(!win.contains(897));
        assert!(!win.contains(1022));
        assert!(!win.contains(1023));
        assert!(win.contains(1024));
        assert!(!win.contains(1025));
        assert!(!win.contains(1026));

        win.insert(std::u64::MAX - 1);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(win.contains(2));
        assert!(win.contains(3));
        assert!(win.contains(4));
        assert!(win.contains(5));
        assert!(win.contains(6));
        assert!(win.contains(7));
        assert!(win.contains(8));
        assert!(win.contains(9));
        assert!(win.contains(10));
        assert!(win.contains(128));
        assert!(win.contains(130));
        assert!(win.contains(132));
        assert!(win.contains(896));
        assert!(win.contains(897));
        assert!(win.contains(1022));
        assert!(win.contains(1023));
        assert!(win.contains(1024));
        assert!(win.contains(1025));
        assert!(win.contains(1026));
        assert!(!win.contains(std::u64::MAX - 2));
        assert!(win.contains(std::u64::MAX - 1));
    }
}
