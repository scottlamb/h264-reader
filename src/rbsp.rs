//! Decoder that will remove _Emulation Prevention_ byte values from encoded NAL Units, to produce
//! the _Raw Byte Sequence Payload_ (RBSP).
//!
//! The following byte sequences are not allowed to appear in a framed H264 bitstream,
//!
//!  - `0x00` `0x00` `0x00`
//!  - `0x00` `0x00` `0x01`
//!  - `0x00` `0x00` `0x02`
//!  - `0x00` `0x00` `0x03`
//!
//! therefore if these byte sequences do appear in the raw bitstream, an 'escaping' mechanism
//! (called 'emulation prevention' in the spec) is applied by adding a `0x03` byte between the
//! second and third bytes in the above sequence, resulting in the following encoded versions,
//!
//!  - `0x00` `0x00` **`0x03`** `0x00`
//!  - `0x00` `0x00` **`0x03`** `0x01`
//!  - `0x00` `0x00` **`0x03`** `0x02`
//!  - `0x00` `0x00` **`0x03`** `0x03`
//!
//! The `RbspDecoder` type will accept byte sequences that have had this encoding applied, and will
//! yield byte sequences where the encoding is removed (i.e. the decoder will replace instances of
//! the sequence `0x00 0x00 0x03` with `0x00 0x00`).

use bitstream_io::read::BitRead as _;
use std::borrow::Cow;
use std::io::BufRead;
use std::io::Read;
use crate::nal::{NalHandler, NalHeader};
use crate::Context;

#[derive(Copy, Clone, Debug)]
enum ParseState {
    Start,
    OneZero,
    TwoZero,
    Skip,
}

/// [`BufRead`] adapter which removes `emulation-prevention-three-byte`s.
/// Typically used via a [`h264_reader::nal::Nal`].
#[derive(Clone)]
pub struct ByteReader<R: BufRead> {
    // self.inner[0..self.i] hasn't yet been emitted and is RBSP (has no
    // emulation_prevention_three_bytes).
    //
    // self.state describes the state before self.inner[self.i].
    //
    // self.inner[self.i..] has yet to be examined.

    inner: R,
    state: ParseState,
    i: usize,
}
impl<R: BufRead> ByteReader<R> {
    /// Constructs an adapter from the given [BufRead]. The caller is expected to have skipped
    /// the NAL header byte already.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            state: ParseState::Skip,
            i: 0,
        }
    }
}
impl<R: BufRead> Read for ByteReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let chunk = self.fill_buf()?;
        let amt = std::cmp::min(buf.len(), chunk.len());
        if amt == 1 {
            // Stolen from std::io::Read implementation for &[u8]:
            // apparently this is faster to special-case.
            buf[0] = chunk[0];
        } else {
            buf[..amt].copy_from_slice(&chunk[..amt]);
        }
        self.consume(amt);
        Ok(amt)
    }
}
impl<R: BufRead> BufRead for ByteReader<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        while self.i == 0 { // slow path
            let chunk = self.inner.fill_buf()?;
            if chunk.is_empty() {
                return Ok(b"");
            }
            if matches!(self.state, ParseState::Skip) {
                self.inner.consume(1);
                self.state = ParseState::Start;
                continue;
            }
            if find_three(&mut self.state, &mut self.i, chunk) {
                self.state = ParseState::Skip;
            }
        }
        Ok(&self.inner.fill_buf()?[0..self.i])
    }

    fn consume(&mut self, amt: usize) {
        self.i = self.i.checked_sub(amt).unwrap();
        self.inner.consume(amt);
    }
}

/// Searches for an emulation_prevention_three_byte, updating `state` and `i` as a side effect.
/// Returns true if one is found; caller needs to further update `state`/`i` then.
/// (The two callers do different things.)
fn find_three(state: &mut ParseState, i: &mut usize, chunk: &[u8]) -> bool {
    while *i < chunk.len() {
        match *state {
            ParseState::Start => match memchr::memchr(0x00, &chunk[*i..]) {
                Some(nonzero_len) => {
                    *i += nonzero_len;
                    *state = ParseState::OneZero;
                },
                None => {
                    *i = chunk.len();
                    break
                },
            },
            ParseState::OneZero => match chunk[*i] {
                0x00 => *state = ParseState::TwoZero,
                _ => *state = ParseState::Start,
            },
            ParseState::TwoZero => match chunk[*i] {
                0x03 => return true,
                0x00 => {
                    eprintln!("RbspDecoder: state={:?}, invalid byte {:#x}", *state, chunk[*i]);
                    *state = ParseState::Start;
                },
                _ => *state = ParseState::Start,
            },
            ParseState::Skip => unreachable!(),
        }
        *i += 1;
    }
    false
}

/// Push parser which removes _emulation prevention_ as it calls
/// an inner [NalHandler]. Expects to be called without the NAL header byte.
pub struct RbspDecoder<R>
    where
        R: NalHandler
{
    state: ParseState,
    nal_reader: R,
}
impl<R> RbspDecoder<R>
    where
        R: NalHandler
{
    pub fn new(nal_reader: R) -> Self {
        RbspDecoder {
            state: ParseState::Start,
            nal_reader,
        }
    }

    fn to(&mut self, new_state: ParseState) {
        self.state = new_state;
    }

    fn emit(&mut self, ctx: &mut Context<R::Ctx>, buf: &[u8]) {
        if !buf.is_empty() {
            self.nal_reader.push(ctx, &buf)
        }
    }

    pub fn handler_ref(&self) -> &R {
        &self.nal_reader
    }

    pub fn into_handler(self) -> R {
        self.nal_reader
    }
}
impl<R> NalHandler for RbspDecoder<R>
    where
        R: NalHandler
{
    type Ctx = R::Ctx;

    fn start(&mut self, ctx: &mut Context<Self::Ctx>, header: NalHeader) {
        self.state = ParseState::Start;
        self.nal_reader.start(ctx, header);
    }

    fn push(&mut self, ctx: &mut Context<Self::Ctx>, mut buf: &[u8]) {
        // buf[0..i] hasn't yet been emitted and is RBSP (has no emulation_prevention_three_bytes).
        // self.state describes the state before buf[i].
        // buf[i..] has yet to be examined.
        let mut i = 0;
        while i < buf.len() {
            if find_three(&mut self.state, &mut i, buf) {
                // i now indexes the emulation_prevention_three_byte.
                let (rbsp, three_onward) = buf.split_at(i);
                self.emit(ctx, rbsp);
                buf = &three_onward[1..];
                i = 0;
                self.state = ParseState::Start;
            }
        }

        // buf is now entirely RBSP.
        self.emit(ctx, buf);
    }

    /// To be invoked when calling code knows that the end of a sequence of NAL Unit data has been
    /// reached.
    ///
    /// For example, if the containing data structure demarcates the end of a sequence of NAL
    /// Units explicitly, the parser for that structure should call `end_units()` once all data
    /// has been passed to the `push()` function.
    fn end(&mut self, ctx: &mut Context<Self::Ctx>) {
        self.to(ParseState::Start);
        self.nal_reader.end(ctx);
    }
}

/// Removes _Emulation Prevention_ from the given byte sequence of a single NAL unit, returning the
/// NAL units _Raw Byte Sequence Payload_ (RBSP). Expects to be called without the NAL header byte.
pub fn decode_nal<'a>(nal_unit: &'a [u8]) -> Cow<'a, [u8]> {
    struct DecoderState<'b> {
        data: Cow<'b, [u8]>,
        index: usize,
    }

    impl<'b> DecoderState<'b> {
        pub fn new(data: Cow<'b, [u8]>) -> Self {
            DecoderState { 
                data,
                index: 0,
            }
        }
    }

    impl<'b> NalHandler for DecoderState<'b> {
        type Ctx = ();

        fn start(&mut self, _ctx: &mut Context<Self::Ctx>, _header: NalHeader) {}

        fn push(&mut self, _ctx: &mut Context<Self::Ctx>, buf: &[u8]) {
            let dest = self.index..(self.index + buf.len());

            if &self.data[dest.clone()] != buf {
                self.data.to_mut()[dest].copy_from_slice(buf);
            }

            self.index += buf.len();
        }

        fn end(&mut self, _ctx: &mut Context<Self::Ctx>) {
            if let Cow::Owned(vec) = &mut self.data {
                vec.truncate(self.index);
            }
        }
    }

    let state = DecoderState::new(Cow::Borrowed(nal_unit));

    let mut decoder = RbspDecoder::new(state);
    let mut ctx = Context::default();

    decoder.push(&mut ctx, nal_unit);
    decoder.end(&mut ctx);

    decoder.into_handler().data
}

#[derive(Debug)]
pub enum BitReaderError {
    ReaderError(std::io::Error),
    ReaderErrorFor(&'static str, std::io::Error),

    /// An Exp-Golomb-coded syntax elements value has more than 32 bits.
    ExpGolombTooLarge(&'static str),
}

pub trait BitRead {
    fn read_ue(&mut self, name: &'static str) -> Result<u32,BitReaderError>;
    fn read_se(&mut self, name: &'static str) -> Result<i32, BitReaderError>;
    fn read_bool(&mut self, name: &'static str) -> Result<bool, BitReaderError>;
    fn read_u8(&mut self, bit_count: u32, name: &'static str) -> Result<u8, BitReaderError>;
    fn read_u16(&mut self, bit_count: u32, name: &'static str) -> Result<u16, BitReaderError>;
    fn read_u32(&mut self, bit_count: u32, name: &'static str) -> Result<u32, BitReaderError>;
    fn read_i32(&mut self, bit_count: u32, name: &'static str) -> Result<i32, BitReaderError>;

    /// Returns true if positioned before the RBSP trailing bits.
    ///
    /// This matches the definition of `more_rbsp_data()` in Rec. ITU-T H.264
    /// (03/2010) section 7.2.
    fn has_more_rbsp_data(&mut self, name: &'static str) -> Result<bool, BitReaderError>;
}

/// Reads H.264 bitstream syntax elements from an RBSP representation (no NAL
/// header byte or emulation prevention three bytes).
pub struct BitReader<R: std::io::BufRead + Clone> {
    reader: bitstream_io::read::BitReader<R, bitstream_io::BigEndian>,
}
impl<R: std::io::BufRead + Clone> BitReader<R> {
    pub fn new(inner: R) -> Self {
        Self { reader: bitstream_io::read::BitReader::new(inner) }
    }
}

impl<R: std::io::BufRead + Clone> BitRead for BitReader<R> {
    fn read_ue(&mut self, name: &'static str) -> Result<u32,BitReaderError> {
        let count = self.reader.read_unary1().map_err(|e| BitReaderError::ReaderErrorFor(name, e))?;
        if count > 31 {
            return Err(BitReaderError::ExpGolombTooLarge(name));
        } else if count > 0 {
            let val = self.read_u32(count, name)?;
            Ok((1 << count) -1 + val)
        } else {
            Ok(0)
        }
    }

    fn read_se(&mut self, name: &'static str) -> Result<i32, BitReaderError> {
        Ok(golomb_to_signed(self.read_ue(name)?))
    }

    fn read_bool(&mut self, name: &'static str) -> Result<bool, BitReaderError> {
        self.reader.read_bit().map_err(|e| BitReaderError::ReaderErrorFor(name, e) )
    }

    fn read_u8(&mut self, bit_count: u32, name: &'static str) -> Result<u8, BitReaderError> {
        self.reader.read(bit_count).map_err(|e| BitReaderError::ReaderErrorFor(name, e))
    }

    fn read_u16(&mut self, bit_count: u32, name: &'static str) -> Result<u16, BitReaderError> {
        self.reader.read(bit_count).map_err(|e| BitReaderError::ReaderErrorFor(name, e))
    }

    fn read_u32(&mut self, bit_count: u32, name: &'static str) -> Result<u32, BitReaderError> {
        self.reader.read(bit_count).map_err(|e| BitReaderError::ReaderErrorFor(name, e))
    }

    fn read_i32(&mut self, bit_count: u32, name: &'static str) -> Result<i32, BitReaderError> {
        self.reader.read(bit_count).map_err(|e| BitReaderError::ReaderErrorFor(name, e))
    }

    fn has_more_rbsp_data(&mut self, name: &'static str) -> Result<bool, BitReaderError> {
        let mut throwaway = self.reader.clone();
        let r = (move || {
            throwaway.skip(1)?;
            throwaway.read_unary1()?;
            Ok::<_, std::io::Error>(())
        })();
        match r {
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(BitReaderError::ReaderErrorFor(name, e)),
            Ok(_) => Ok(true),
        }
    }
}
fn golomb_to_signed(val: u32) -> i32 {
    let sign = (((val & 0x1) as i32) << 1) - 1;
    ((val >> 1) as i32 + (val & 0x1) as i32) * sign
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::cell::RefCell;
    use hex_literal::*;
    use hex_slice::AsHex;

    struct State {
        started: bool,
        ended: bool,
        data: Vec<u8>,
    }
    struct MockReader {
        state: Rc<RefCell<State>>
    }
    impl MockReader {
        fn new(state: Rc<RefCell<State>>) -> MockReader {
            MockReader {
                state
            }
        }
    }
    impl NalHandler for MockReader {
        type Ctx = ();

        fn start(&mut self, _ctx: &mut Context<Self::Ctx>, _header: NalHeader) {
            self.state.borrow_mut().started = true;
        }

        fn push(&mut self, _ctx: &mut Context<Self::Ctx>, buf: &[u8]) {
            self.state.borrow_mut().data.extend_from_slice(buf);
        }

        fn end(&mut self, _ctx: &mut Context<Self::Ctx>) {
            self.state.borrow_mut().ended = true;
        }
    }

    #[test]
    fn push_decoder() {
        let data = hex!(
           "67 64 00 0A AC 72 84 44 26 84 00 00 03
            00 04 00 00 03 00 CA 3C 48 96 11 80");
        for i in 1..data.len()-1 {
            let state = Rc::new(RefCell::new(State {
                started: false,
                ended: false,
                data: Vec::new(),
            }));
            let mock = MockReader::new(Rc::clone(&state));
            let mut r = RbspDecoder::new(mock);
            let mut ctx = Context::default();
            let (head, tail) = data.split_at(i);
            r.push(&mut ctx, head);
            r.push(&mut ctx, tail);
            let expected = hex!(
           "67 64 00 0A AC 72 84 44 26 84 00 00
            00 04 00 00 00 CA 3C 48 96 11 80");
            let s = state.borrow();
            assert_eq!(&s.data[..], &expected[..], "on split_at({})", i);
        }
    }

    #[test]
    fn byte_reader() {
        let data = hex!(
           "67 64 00 0A AC 72 84 44 26 84 00 00 03
            00 04 00 00 03 00 CA 3C 48 96 11 80");
        for i in 1..data.len()-1 {
            let (head, tail) = data.split_at(i);
            let r = head.chain(tail);
            let mut r = ByteReader::new(r);
            let mut rbsp = Vec::new();
            r.read_to_end(&mut rbsp).unwrap();
            let expected = hex!(
           "64 00 0A AC 72 84 44 26 84 00 00
            00 04 00 00 00 CA 3C 48 96 11 80");
            assert!(rbsp == &expected[..],
                    "Mismatch with on split_at({}):\nrbsp     {:02x}\nexpected {:02x}",
                    i, rbsp.as_hex(), expected.as_hex());
        }
    }

    #[test]
    fn decode_single_nal() {
        let data = hex!(
           "67 42 c0 15 d9 01 41 fb 01 6a 0c 02 0b
            4a 00 00 03 00 02 00 00 03 00 79 1e 2c
            5c 90");
        let expected = hex!(
           "67 42 c0 15 d9 01 41 fb 01 6a 0c 02 0b
            4a 00 00 00 02 00 00 00 79 1e 2c 5c 90");

        let decoded = decode_nal(&data);

        assert_eq!(decoded, &expected[..]);
        assert!(matches!(decoded, Cow::Owned(..)));
    }

    #[test]
    fn decode_single_nal_no_emulation() {
        let data = hex!(
           "64 00 0A AC 72 84 44 26 84 00 00
            00 04 00 00 00 CA 3C 48 96 11 80");
        let expected = hex!(
           "64 00 0A AC 72 84 44 26 84 00 00
            00 04 00 00 00 CA 3C 48 96 11 80");

        let decoded = decode_nal(&data);

        assert_eq!(decoded, &expected[..]);
        assert!(matches!(decoded, Cow::Borrowed(..)));
    }

    #[test]
    fn bitreader_has_more_data() {
        // Should work when the end bit is byte-aligned.
        let mut reader = BitReader::new(&[0x12, 0x80][..]);
        assert!(reader.has_more_rbsp_data("call 1").unwrap());
        assert_eq!(reader.read_u8(8, "u8 1").unwrap(), 0x12);
        assert!(!reader.has_more_rbsp_data("call 2").unwrap());

        // and when it's not.
        let mut reader = BitReader::new(&[0x18][..]);
        assert!(reader.has_more_rbsp_data("call 3").unwrap());
        assert_eq!(reader.read_u8(4, "u8 2").unwrap(), 0x1);
        assert!(!reader.has_more_rbsp_data("call 4").unwrap());

        // should also work when there are cabac-zero-words.
        let mut reader = BitReader::new(&[0x80, 0x00, 0x00][..]);
        assert!(!reader.has_more_rbsp_data("at end with cabac-zero-words").unwrap());
    }

    #[test]
    fn read_ue_overflow() {
        let mut reader = BitReader::new(&[0, 0, 0, 0, 255, 255, 255, 255, 255][..]);
        assert!(matches!(reader.read_ue("test"), Err(BitReaderError::ExpGolombTooLarge("test"))));
    }
}
