//! The scene channel of File Commander's sessions on the wire: protobuf
//! messages in a type-length-value stream over the session's Unix socket.
//!
//! ```text
//! message := type:varint  length:varint  payload:length bytes
//! ```
//!
//! Varints are protobuf's (unsigned LEB128, at most ten bytes, minimal). The
//! payload is one message of `scene.proto` (package `fresh.scene.v1`), of the
//! kind `type` names ([`kind`]).
//!
//! Messages are written by hand rather than through generated types, which
//! own a `String` or a `Vec` for every field: the encoders here read the views
//! and the cells in place and write into buffers kept between frames. Each
//! message is declared once ([`proto!`], [`proto_oneof!`]), which generates
//! both its encoder and its `.proto` text; a test keeps `scene.proto` equal to
//! [`schema`], and others decode the output with types generated from it.
//!
//! There is no JSON on this path, with one exception: the arguments a menu
//! item's action carries are free-form JSON in the keymap, and travel as that
//! JSON (`args_json`), which the app hands back untouched.

use std::collections::HashMap;
use std::fmt;

/// What a message is, its `type` on the wire.
pub(crate) mod kind {
    pub const SERVER_HELLO: u64 = 1;
    pub const FRAME: u64 = 2;
    pub const ERROR: u64 = 3;
    pub const CLIENT_HELLO: u64 = 16;
    pub const KEY: u64 = 17;
    pub const MOUSE: u64 = 18;
    pub const RESIZE: u64 = 19;
    pub const PASTE: u64 = 20;
    pub const ACTION: u64 = 21;
    pub const WIDGET: u64 = 22;
    pub const SETTINGS_CLICK: u64 = 23;
    pub const KB_EDIT_CLICK: u64 = 24;
    pub const CONFIGURE: u64 = 25;
    pub const APPLIED: u64 = 26;
    pub const COMPOSE_INPUT: u64 = 27;
    pub const COMPOSE_ASSET: u64 = 28;
    pub const COMPOSE_VIEWPORT_REQUEST: u64 = 29;
}

/// The protocol version a client's hello must name.
pub(crate) const VERSION: u32 = 1;

/// The longest payload either side accepts.
pub(crate) const MAX_PAYLOAD: usize = 16 << 20;

const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const LEN: u8 = 2;
const FIXED32: u8 = 5;

// ─────────────────────────── varints and TLV ───────────────────────────

/// The bytes a varint takes.
#[inline]
pub(crate) fn varint_len(v: u64) -> usize {
    ((64 - (v | 1).leading_zeros()) as usize).div_ceil(7)
}

#[inline]
pub(crate) fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push(v as u8 | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// Why bytes off the wire were refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WireError {
    /// A varint longer than ten bytes, or past 64 bits.
    LongVarint,
    /// A varint with trailing zero groups.
    NotMinimal,
    /// A payload longer than [`MAX_PAYLOAD`].
    TooLong(u64),
    /// A field running past the end of its message.
    Truncated,
    /// A protobuf wire type that isn't one.
    WireType(u8),
    /// A string field that isn't UTF-8.
    Utf8,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::LongVarint => write!(f, "varint longer than ten bytes"),
            WireError::NotMinimal => write!(f, "varint not minimally encoded"),
            WireError::TooLong(n) => write!(f, "payload of {n} bytes, more than {MAX_PAYLOAD}"),
            WireError::Truncated => write!(f, "field runs past the end of its message"),
            WireError::WireType(t) => write!(f, "unknown wire type {t}"),
            WireError::Utf8 => write!(f, "string field is not UTF-8"),
        }
    }
}

/// The varint at the start of `bytes` and the bytes it took; `None` while it
/// hasn't all arrived.
pub(crate) fn read_varint(bytes: &[u8]) -> Result<Option<(u64, usize)>, WireError> {
    let mut value = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        // The tenth byte holds the 64th bit and nothing else.
        if i == 9 && b > 1 {
            return Err(WireError::LongVarint);
        }
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            if i > 0 && b == 0 {
                return Err(WireError::NotMinimal);
            }
            return Ok(Some((value, i + 1)));
        }
    }
    Ok(None)
}

/// A message's header: its type and its payload's length.
pub(crate) fn put_header(buf: &mut Vec<u8>, kind: u64, len: usize) {
    put_varint(buf, kind);
    put_varint(buf, len as u64);
}

/// The next message at the start of `bytes`: its type, where its payload is,
/// and the bytes the whole message took; `None` while it hasn't all arrived.
pub(crate) fn read_message(
    bytes: &[u8],
) -> Result<Option<(u64, std::ops::Range<usize>, usize)>, WireError> {
    let Some((kind, a)) = read_varint(bytes)? else {
        return Ok(None);
    };
    let Some((len, b)) = read_varint(&bytes[a..])? else {
        return Ok(None);
    };
    if len > MAX_PAYLOAD as u64 {
        return Err(WireError::TooLong(len));
    }
    let start = a + b;
    let end = start + len as usize;
    if bytes.len() < end {
        return Ok(None);
    }
    Ok(Some((kind, start..end, end)))
}

// ─────────────────────────── protobuf, writing ───────────────────────────

/// A protobuf message this side writes: the length of its fields, then the
/// fields (no length prefix of its own).
pub(crate) trait Message {
    fn encoded_len(&self) -> usize;
    fn encode(&self, buf: &mut Vec<u8>);
}

impl<T: Message + ?Sized> Message for &T {
    fn encoded_len(&self) -> usize {
        (**self).encoded_len()
    }
    fn encode(&self, buf: &mut Vec<u8>) {
        (**self).encode(buf)
    }
}

impl<T: Message + ?Sized> Message for Box<T> {
    fn encoded_len(&self) -> usize {
        (**self).encoded_len()
    }
    fn encode(&self, buf: &mut Vec<u8>) {
        (**self).encode(buf)
    }
}

/// An unsigned integer field's value (`uint32`, `uint64`, `bool`).
pub(crate) trait Uint {
    fn uint(&self) -> u64;
}

macro_rules! uint_from {
    ($($t:ty),*) => {$(
        impl Uint for $t {
            #[inline]
            fn uint(&self) -> u64 {
                *self as u64
            }
        }
    )*};
}
uint_from!(u8, u16, u32, u64, usize);

impl Uint for bool {
    #[inline]
    fn uint(&self) -> u64 {
        u64::from(*self)
    }
}

/// A color, `0xRRGGBB`.
impl Uint for crate::view::scene::Rgb {
    #[inline]
    fn uint(&self) -> u64 {
        u64::from(self.packed())
    }
}

impl<T: Uint + ?Sized> Uint for &T {
    #[inline]
    fn uint(&self) -> u64 {
        (**self).uint()
    }
}

/// A `double` field's value.
pub(crate) trait Double {
    fn double(&self) -> f64;
}

impl Double for f64 {
    #[inline]
    fn double(&self) -> f64 {
        *self
    }
}

impl<T: Double + ?Sized> Double for &T {
    #[inline]
    fn double(&self) -> f64 {
        (**self).double()
    }
}

/// A `string` field's value.
pub(crate) trait Text {
    fn text(&self) -> &str;
}

impl Text for str {
    #[inline]
    fn text(&self) -> &str {
        self
    }
}

impl Text for String {
    #[inline]
    fn text(&self) -> &str {
        self
    }
}

impl<T: Text + ?Sized> Text for &T {
    #[inline]
    fn text(&self) -> &str {
        (**self).text()
    }
}

#[inline]
fn key_len(tag: u32) -> usize {
    varint_len(u64::from(tag) << 3)
}

#[inline]
fn put_key(buf: &mut Vec<u8>, tag: u32, wire: u8) {
    put_varint(buf, u64::from(tag) << 3 | u64::from(wire));
}

/// Field writers and their lengths, one pair per kind of field. Proto3
/// leaves a plain field's default (0, false, "", nothing) out; an `optional`
/// one is written whenever it is there.
pub(crate) mod field {
    use super::*;

    #[inline]
    pub(crate) fn uint_len(tag: u32, v: u64) -> usize {
        if v == 0 {
            0
        } else {
            key_len(tag) + varint_len(v)
        }
    }

    #[inline]
    pub(crate) fn uint(buf: &mut Vec<u8>, tag: u32, v: u64) {
        if v != 0 {
            put_key(buf, tag, VARINT);
            put_varint(buf, v);
        }
    }

    #[inline]
    pub(crate) fn opt_uint_len(tag: u32, v: Option<u64>) -> usize {
        v.map_or(0, |v| key_len(tag) + varint_len(v))
    }

    #[inline]
    pub(crate) fn opt_uint(buf: &mut Vec<u8>, tag: u32, v: Option<u64>) {
        if let Some(v) = v {
            put_key(buf, tag, VARINT);
            put_varint(buf, v);
        }
    }

    #[inline]
    pub(crate) fn double_len(tag: u32, v: f64) -> usize {
        if v.to_bits() == 0 {
            0
        } else {
            key_len(tag) + 8
        }
    }

    #[inline]
    pub(crate) fn double(buf: &mut Vec<u8>, tag: u32, v: f64) {
        if v.to_bits() != 0 {
            put_key(buf, tag, FIXED64);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    #[inline]
    pub(crate) fn opt_double_len(tag: u32, v: Option<f64>) -> usize {
        v.map_or(0, |_| key_len(tag) + 8)
    }

    #[inline]
    pub(crate) fn opt_double(buf: &mut Vec<u8>, tag: u32, v: Option<f64>) {
        if let Some(v) = v {
            put_key(buf, tag, FIXED64);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    #[inline]
    pub(crate) fn bytes_len(tag: u32, n: usize) -> usize {
        key_len(tag) + varint_len(n as u64) + n
    }

    #[inline]
    pub(crate) fn text_len(tag: u32, s: &str) -> usize {
        if s.is_empty() {
            0
        } else {
            bytes_len(tag, s.len())
        }
    }

    #[inline]
    pub(crate) fn text(buf: &mut Vec<u8>, tag: u32, s: &str) {
        if !s.is_empty() {
            raw_bytes(buf, tag, s.as_bytes());
        }
    }

    #[inline]
    pub(crate) fn raw_bytes(buf: &mut Vec<u8>, tag: u32, b: &[u8]) {
        put_key(buf, tag, LEN);
        put_varint(buf, b.len() as u64);
        buf.extend_from_slice(b);
    }

    #[inline]
    pub(crate) fn opt_text_len(tag: u32, s: Option<&str>) -> usize {
        s.map_or(0, |s| bytes_len(tag, s.len()))
    }

    #[inline]
    pub(crate) fn opt_text(buf: &mut Vec<u8>, tag: u32, s: Option<&str>) {
        if let Some(s) = s {
            raw_bytes(buf, tag, s.as_bytes());
        }
    }

    /// Repeated strings: every element, empty ones too.
    pub(crate) fn texts_len<'a>(tag: u32, items: impl Iterator<Item = &'a str>) -> usize {
        items.map(|s| bytes_len(tag, s.len())).sum()
    }

    pub(crate) fn texts<'a>(buf: &mut Vec<u8>, tag: u32, items: impl Iterator<Item = &'a str>) {
        for s in items {
            raw_bytes(buf, tag, s.as_bytes());
        }
    }

    /// Repeated numbers, packed: one length-delimited run of varints, left out
    /// when there are none.
    pub(crate) fn packed_len(tag: u32, values: impl Iterator<Item = u64>) -> usize {
        let (count, payload) = values.fold((0usize, 0usize), |(c, p), v| (c + 1, p + varint_len(v)));
        if count == 0 {
            0
        } else {
            bytes_len(tag, payload)
        }
    }

    pub(crate) fn packed(buf: &mut Vec<u8>, tag: u32, values: impl Iterator<Item = u64> + Clone) {
        let payload: usize = values.clone().map(varint_len).sum();
        if payload == 0 {
            return;
        }
        put_key(buf, tag, LEN);
        put_varint(buf, payload as u64);
        for v in values {
            put_varint(buf, v);
        }
    }

    #[inline]
    pub(crate) fn msg_len<M: Message + ?Sized>(tag: u32, m: &M) -> usize {
        bytes_len(tag, m.encoded_len())
    }

    #[inline]
    pub(crate) fn msg<M: Message + ?Sized>(buf: &mut Vec<u8>, tag: u32, m: &M) {
        put_key(buf, tag, LEN);
        put_varint(buf, m.encoded_len() as u64);
        m.encode(buf);
    }

    #[inline]
    pub(crate) fn opt_msg_len<M: Message>(tag: u32, m: Option<&M>) -> usize {
        m.map_or(0, |m| msg_len(tag, m))
    }

    #[inline]
    pub(crate) fn opt_msg<M: Message>(buf: &mut Vec<u8>, tag: u32, m: Option<&M>) {
        if let Some(m) = m {
            msg(buf, tag, m);
        }
    }

    pub(crate) fn msgs_len<'a, M: Message + 'a>(tag: u32, items: impl Iterator<Item = &'a M>) -> usize {
        items.map(|m| msg_len(tag, m)).sum()
    }

    pub(crate) fn msgs<'a, M: Message + 'a>(buf: &mut Vec<u8>, tag: u32, items: impl Iterator<Item = &'a M>) {
        for m in items {
            msg(buf, tag, m);
        }
    }

    /// Action arguments, the one field that is JSON: free-form in the keymap,
    /// opaque to the front end, which hands them back. Keys in order, so the
    /// same arguments always encode the same; left out when there are none.
    pub(crate) fn json_len(tag: u32, args: &HashMap<String, serde_json::Value>) -> usize {
        if args.is_empty() {
            return 0;
        }
        let mut counter = Counter(0);
        write_args(&mut counter, args);
        bytes_len(tag, counter.0)
    }

    pub(crate) fn json(buf: &mut Vec<u8>, tag: u32, args: &HashMap<String, serde_json::Value>) {
        if args.is_empty() {
            return;
        }
        let mut counter = Counter(0);
        write_args(&mut counter, args);
        put_key(buf, tag, LEN);
        put_varint(buf, counter.0 as u64);
        write_args(buf, args);
    }

    /// Counts the bytes written to it.
    struct Counter(usize);

    impl std::io::Write for Counter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The arguments as a JSON object, keys in order. Up to eight are sorted
    /// on the stack, so a menu's arguments, encoded every frame, allocate
    /// nothing.
    fn write_args(out: &mut impl std::io::Write, args: &HashMap<String, serde_json::Value>) {
        let mut small: [Option<(&String, &serde_json::Value)>; 8] = [None; 8];
        let mut large = Vec::new();
        let entries: &mut [Option<(&String, &serde_json::Value)>] = if args.len() <= small.len() {
            for (slot, entry) in small.iter_mut().zip(args.iter()) {
                *slot = Some(entry);
            }
            &mut small[..args.len()]
        } else {
            large.extend(args.iter().map(Some));
            &mut large
        };
        entries.sort_unstable_by(|a, b| a.map(|e| e.0).cmp(&b.map(|e| e.0)));
        // Writing to memory or a counter never fails, and a JSON value always
        // serializes.
        out.write_all(b"{").unwrap_or_default();
        for (i, (key, value)) in entries.iter().flatten().enumerate() {
            if i > 0 {
                out.write_all(b",").unwrap_or_default();
            }
            serde_json::to_writer(&mut *out, key).unwrap_or_default();
            out.write_all(b":").unwrap_or_default();
            serde_json::to_writer(&mut *out, value).unwrap_or_default();
        }
        out.write_all(b"}").unwrap_or_default();
    }
}

/// Declares protobuf messages for Rust structs: each field's number, name and
/// type, in `.proto` terms. Generates the struct's [`Message`] encoder and the
/// message's `.proto` text, collected in the `SCHEMA` constant this defines.
///
/// Field types: `uint32`, `uint64`, `bool`, `double`, `string`; the
/// same after `optional` for a Rust `Option`; `repeated` before a number type
/// (packed), `string` or a message for a `Vec`; a message name for a
/// struct field, or after `maybe` for an `Option` of one (written as the
/// message type); `json` for action arguments (written as `string`). A
/// struct or a field can take another name in the schema (`rust as proto`).
macro_rules! proto {
    // ---- field lengths
    (@len $t:literal, $v:expr, uint32) => { field::uint_len($t, Uint::uint(&$v)) };
    (@len $t:literal, $v:expr, uint64) => { field::uint_len($t, Uint::uint(&$v)) };
    (@len $t:literal, $v:expr, bool) => { field::uint_len($t, Uint::uint(&$v)) };
    (@len $t:literal, $v:expr, double) => { field::double_len($t, Double::double(&$v)) };
    (@len $t:literal, $v:expr, string) => { field::text_len($t, Text::text(&$v)) };
    (@len $t:literal, $v:expr, bytes) => { field::bytes_len($t, $v.len()) };
    (@len $t:literal, $v:expr, json) => { field::json_len($t, &$v) };
    (@len $t:literal, $v:expr, optional double) => { field::opt_double_len($t, $v.as_ref().map(|v| Double::double(v))) };
    (@len $t:literal, $v:expr, optional string) => { field::opt_text_len($t, $v.as_ref().map(|v| Text::text(v))) };
    (@len $t:literal, $v:expr, optional $k:ident) => { field::opt_uint_len($t, $v.as_ref().map(|v| Uint::uint(v))) };
    (@len $t:literal, $v:expr, repeated string) => { field::texts_len($t, $v.iter().map(|v| Text::text(v))) };
    (@len $t:literal, $v:expr, repeated uint32) => { field::packed_len($t, $v.iter().map(|v| Uint::uint(v))) };
    (@len $t:literal, $v:expr, repeated uint64) => { field::packed_len($t, $v.iter().map(|v| Uint::uint(v))) };
    (@len $t:literal, $v:expr, repeated $m:ident) => { field::msgs_len($t, $v.iter()) };
    (@len $t:literal, $v:expr, maybe $m:ident) => { field::opt_msg_len($t, $v.as_ref()) };
    (@len $t:literal, $v:expr, $m:ident) => { field::msg_len($t, &$v) };
    // ---- field writers
    (@put $b:ident, $t:literal, $v:expr, uint32) => { field::uint($b, $t, Uint::uint(&$v)) };
    (@put $b:ident, $t:literal, $v:expr, uint64) => { field::uint($b, $t, Uint::uint(&$v)) };
    (@put $b:ident, $t:literal, $v:expr, bool) => { field::uint($b, $t, Uint::uint(&$v)) };
    (@put $b:ident, $t:literal, $v:expr, double) => { field::double($b, $t, Double::double(&$v)) };
    (@put $b:ident, $t:literal, $v:expr, string) => { field::text($b, $t, Text::text(&$v)) };
    (@put $b:ident, $t:literal, $v:expr, bytes) => { field::raw_bytes($b, $t, &$v) };
    (@put $b:ident, $t:literal, $v:expr, json) => { field::json($b, $t, &$v) };
    (@put $b:ident, $t:literal, $v:expr, optional double) => { field::opt_double($b, $t, $v.as_ref().map(|v| Double::double(v))) };
    (@put $b:ident, $t:literal, $v:expr, optional string) => { field::opt_text($b, $t, $v.as_ref().map(|v| Text::text(v))) };
    (@put $b:ident, $t:literal, $v:expr, optional $k:ident) => { field::opt_uint($b, $t, $v.as_ref().map(|v| Uint::uint(v))) };
    (@put $b:ident, $t:literal, $v:expr, repeated string) => { field::texts($b, $t, $v.iter().map(|v| Text::text(v))) };
    (@put $b:ident, $t:literal, $v:expr, repeated uint32) => { field::packed($b, $t, $v.iter().map(|v| Uint::uint(v))) };
    (@put $b:ident, $t:literal, $v:expr, repeated uint64) => { field::packed($b, $t, $v.iter().map(|v| Uint::uint(v))) };
    (@put $b:ident, $t:literal, $v:expr, repeated $m:ident) => { field::msgs($b, $t, $v.iter()) };
    (@put $b:ident, $t:literal, $v:expr, maybe $m:ident) => { field::opt_msg($b, $t, $v.as_ref()) };
    (@put $b:ident, $t:literal, $v:expr, $m:ident) => { field::msg($b, $t, &$v) };
    // ---- .proto text
    (@ptype json) => { "string" };
    (@ptype maybe $m:ident) => { stringify!($m) };
    (@ptype optional $k:ident) => { concat!("optional ", stringify!($k)) };
    (@ptype repeated $k:ident) => { concat!("repeated ", stringify!($k)) };
    (@ptype $k:ident) => { stringify!($k) };
    (@name $r:ident $p:ident) => { stringify!($p) };
    (@name $r:ident) => { stringify!($r) };
    // ---- the messages
    ($(
        $(#[doc = $doc:literal])*
        $rust:ident $(as $pname:ident)? {
            $( $tag:literal $field:ident $(as $pfield:ident)? : $($kind:ident)+ ),* $(,)?
        }
    )*) => {
        $(
            impl Message for $rust {
                #[allow(unused_variables)]
                fn encoded_len(&self) -> usize {
                    0 $( + proto!(@len $tag, self.$field, $($kind)+) )*
                }
                #[allow(unused_variables)]
                fn encode(&self, buf: &mut Vec<u8>) {
                    $( proto!(@put buf, $tag, self.$field, $($kind)+); )*
                }
            }
        )*
        /// The messages above, as `.proto` text.
        pub(crate) const SCHEMA: &[&str] = &[$(
            concat!(
                $("//", $doc, "\n",)*
                "message ", proto!(@name $rust $($pname)?), " {\n",
                $("  ", proto!(@ptype $($kind)+), " ", proto!(@name $field $($pfield)?), " = ", $tag, ";\n",)*
                "}\n"
            )
        ),*];
    };
}

/// Declares protobuf messages for Rust enums whose variants carry fields: a
/// message per variant, and one for the enum holding them in a `oneof`.
/// Fields as in [`proto!`]; fields a variant has but doesn't list stay off
/// the wire.
macro_rules! proto_oneof {
    ($(
        $(#[doc = $doc:literal])*
        $rust:ident as $pname:ident, $oneof:ident {
            $(
                $vtag:literal $vname:ident : $variant:ident as $vproto:ident {
                    $( $tag:literal $field:ident $(as $pfield:ident)? : $($kind:ident)+ ),* $(,)?
                }
            ),* $(,)?
        }
    )*) => {
        $(
            impl Message for $rust {
                #[allow(unused_variables)]
                fn encoded_len(&self) -> usize {
                    match self {
                        $(
                            $rust::$variant { $($field,)* .. } => {
                                let inner = 0 $( + proto!(@len $tag, $field, $($kind)+) )*;
                                field::bytes_len($vtag, inner)
                            }
                        )*
                    }
                }
                #[allow(unused_variables)]
                fn encode(&self, buf: &mut Vec<u8>) {
                    match self {
                        $(
                            $rust::$variant { $($field,)* .. } => {
                                let inner = 0 $( + proto!(@len $tag, $field, $($kind)+) )*;
                                put_key(buf, $vtag, LEN);
                                put_varint(buf, inner as u64);
                                $( proto!(@put buf, $tag, $field, $($kind)+); )*
                            }
                        )*
                    }
                }
            }
        )*
        /// The messages above, as `.proto` text.
        pub(crate) const ONEOF_SCHEMA: &[&str] = &[$(
            concat!(
                $(
                    "message ", stringify!($vproto), " {\n",
                    $("  ", proto!(@ptype $($kind)+), " ", proto!(@name $field $($pfield)?), " = ", $tag, ";\n",)*
                    "}\n",
                )*
                $("//", $doc, "\n",)*
                "message ", stringify!($pname), " {\n",
                "  oneof ", stringify!($oneof), " {\n",
                $("    ", stringify!($vproto), " ", stringify!($vname), " = ", $vtag, ";\n",)*
                "  }\n",
                "}\n"
            )
        ),*];
    };
}


// ─────────────────────────── protobuf, reading ───────────────────────────

/// A field's value off the wire.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

impl<'a> Field<'a> {
    pub(crate) fn uint(self) -> u64 {
        match self {
            Field::Varint(v) | Field::Fixed64(v) => v,
            Field::Fixed32(v) => u64::from(v),
            Field::Bytes(_) => 0,
        }
    }

    pub(crate) fn flag(self) -> bool {
        self.uint() != 0
    }

    pub(crate) fn text(self) -> Result<&'a str, WireError> {
        match self {
            Field::Bytes(b) => std::str::from_utf8(b).map_err(|_| WireError::Utf8),
            _ => Ok(""),
        }
    }
}

/// The fields of one message, in order.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = Result<(u32, Field<'a>), WireError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let mut read = || -> Result<(u32, Field<'a>), WireError> {
            let (key, n) = read_varint(self.bytes)?.ok_or(WireError::Truncated)?;
            self.bytes = &self.bytes[n..];
            let tag = (key >> 3) as u32;
            let field = match (key & 7) as u8 {
                VARINT => {
                    let (v, n) = read_varint(self.bytes)?.ok_or(WireError::Truncated)?;
                    self.bytes = &self.bytes[n..];
                    Field::Varint(v)
                }
                FIXED64 => {
                    let (head, rest) = self.bytes.split_at_checked(8).ok_or(WireError::Truncated)?;
                    self.bytes = rest;
                    Field::Fixed64(u64::from_le_bytes(head.try_into().unwrap_or_default()))
                }
                LEN => {
                    let (len, n) = read_varint(self.bytes)?.ok_or(WireError::Truncated)?;
                    let rest = &self.bytes[n..];
                    let len = usize::try_from(len).map_err(|_| WireError::Truncated)?;
                    let (head, rest) = rest.split_at_checked(len).ok_or(WireError::Truncated)?;
                    self.bytes = rest;
                    Field::Bytes(head)
                }
                FIXED32 => {
                    let (head, rest) = self.bytes.split_at_checked(4).ok_or(WireError::Truncated)?;
                    self.bytes = rest;
                    Field::Fixed32(u32::from_le_bytes(head.try_into().unwrap_or_default()))
                }
                other => return Err(WireError::WireType(other)),
            };
            Ok((tag, field))
        };
        let item = read();
        if item.is_err() {
            self.bytes = &[];
        }
        Some(item)
    }
}

// ─────────────────────────── the scene's messages ───────────────────────────
//
// What File Commander's native editor reads of the scene, and nothing more:
// the regions it draws natively or acts on. The plugin panels (`tree`), the
// menu dropdown's state, the auxiliary modals, the palette's plugin toolbar,
// preview cells and file browser, and the pacing hint stay off the wire; the
// grid's cells carry what the app shows of them.

use super::{PaneModel, SceneTheme, ScrollbarMarker, SeparatorModel};
use super::compose::{ComposePane, ComposeSegment, ComposeSpan, ComposeImage, ComposeSelection};
use super::compose_assets::Chunk;
use super::compose_viewport::{Value as ComposeViewport, Reply as ComposeViewportReply};
use super::compose::InputReply;
use super::compose_navigation::CaretState;
use crate::view::scene::{
    ContextMenuView, EntryDialogView, FileExplorerView, FileRow, KbConfirm, KbEditDialog, KbRow,
    KbSearchView, KeybindingEditorView, MapEntryView, MenuEntry, MenuItemView, PaletteView,
    PopupContentView, PopupItemView, PopupSpanView, RectView, ScenePopup, SearchOptionView,
    SearchOptionsView, SettingControlView, SettingItemView, SettingsCategoryView,
    SettingsSearchResultView, SettingsView, StatusSegment, StatusView, SuggestionView, TabBarView,
    TabView, TrustDialogView, TrustOptionView,
};

/// A frame's timing, for the front end's latency telemetry (telemetry on).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Timing {
    pub(crate) ack: u64,
    pub(crate) build_ms: f64,
    pub(crate) render_ms: f64,
    pub(crate) grid_ms: f64,
    pub(crate) encode_ms: f64,
    pub(crate) queued: u64,
    pub(crate) ahead: u64,
    pub(crate) owed: u32,
    pub(crate) top: u64,
    pub(crate) cpu_ms: f64,
    pub(crate) prev_allocs: Option<u64>,
    pub(crate) prev_alloc_bytes: Option<u64>,
}

proto! {
    /// A source marker and a caret-fragment offset in native points, per client.
    ComposeViewport { 1 position: uint64, 2 affinity: uint32, 3 offset: double, 4 sequence: uint64, 5 epoch: uint64 }
    ComposeViewportReply { 1 id: uint64, 2 accepted: bool }
    /// A bounded binary image payload, multiplexed with normal input/scene frames.
    Chunk as ComposeAssetChunk {
        1 id: uint64, 2 offset: uint64, 3 total: uint64, 4 data: bytes,
        5 error: string, 6 done: bool,
    }
    /// A rectangle of cells.
    RectView as Rect { 1 x: uint32, 2 y: uint32, 3 w: uint32, 4 h: uint32 }
    /// A top-level menu and its items.
    MenuEntry {
        1 label: string, 2 visible: bool, 3 x: optional uint32, 4 w: optional uint32,
        5 items: repeated MenuItem,
    }
    TabView {
        1 buffer_id: optional uint64, 2 label: string, 3 active: bool, 4 modified: bool,
        5 rect: Rect, 6 close_rect: Rect,
    }
    TabBarView { 1 bar: maybe Rect, 2 tabs: repeated TabView }
    StatusSegment {
        1 name: string, 2 key: optional string, 3 text: string, 4 x: uint32, 5 w: uint32,
        6 side: string,
    }
    StatusView { 1 rect: Rect, 2 segments: repeated StatusSegment }
    SuggestionView {
        1 text: string, 2 description: optional string, 3 keybinding: optional string,
        4 disabled: bool,
    }
    SearchOptionView {
        1 name: string, 2 label: string, 3 shortcut: optional string, 4 active: bool,
        5 x: uint32, 6 w: uint32,
    }
    SearchOptionsView { 1 row: uint32, 2 options: repeated SearchOptionView }
    /// The command palette or another prompt, without its plugin toolbar.
    PaletteView {
        1 query: string, 2 message: string, 3 prompt_type: string, 4 overlay: bool,
        5 title: string, 6 status: string, 7 selected: optional uint64, 8 scroll_start: uint64,
        9 visible_count: uint64, 10 total: uint64, 11 outer_rect: maybe Rect,
        12 list_rect: maybe Rect, 13 preview_rect: maybe Rect,
        14 suggestions: repeated SuggestionView, 15 search_options: maybe SearchOptionsView,
    }
    PopupItemView {
        1 text: string, 2 detail: optional string, 3 icon: optional string, 4 disabled: bool,
    }
    /// A styled span of a popup's line; colors 0xRRGGBB, left out for the default.
    PopupSpanView {
        1 text: string, 2 fg: optional uint32, 3 bg: optional uint32, 4 bold: bool,
        5 italic: bool, 6 underline: bool, 7 reverse: bool,
    }
    ScenePopup {
        1 kind: string, 2 title: optional string, 3 description: optional string,
        4 rect: Rect, 5 content_rect: Rect, 6 scroll_offset: uint64, 7 content: PopupContent,
    }
    FileRow { 1 name: string, 2 depth: uint64, 3 is_dir: bool, 4 expanded: bool }
    FileExplorerView {
        1 rect: Rect, 2 title: string, 3 scroll_offset: uint64, 4 viewport_height: uint64,
        5 selected: optional uint64, 6 viewport_rows: repeated uint64, 7 rows: repeated FileRow,
    }
    TrustOptionView {
        1 label: string, 2 description: string, 3 selected: bool, 4 data: string, 5 rect: Rect,
    }
    TrustDialogView {
        1 dialog: Rect, 2 title: string, 3 path: string, 4 triggers: string,
        5 cancellable: bool, 6 options: repeated TrustOptionView, 7 ok: Rect,
        8 ok_label: string, 9 quit: Rect, 10 quit_label: string,
    }
    ContextMenuView {
        1 kind: string, 2 x: uint32, 3 y: uint32, 4 highlighted: uint64,
        5 items: repeated string,
    }
    KbSearchView {
        1 active: bool, 2 focused: bool, 3 mode: string, 4 query: string,
        5 key_display: string,
    }
    KbEditDialog {
        1 title: string, 2 focus_area: uint64, 3 key_display: string, 4 key_capturing: bool,
        5 action_text: string, 6 action_error: optional string,
        7 autocomplete: repeated string, 8 autocomplete_selected: optional uint64,
        9 context: string, 10 context_options: repeated string, 11 conflicts: repeated string,
        12 save_focused: bool, 13 cancel_focused: bool,
    }
    KbConfirm { 1 buttons: repeated string, 2 selected: uint64 }
    KeybindingEditorView {
        1 title: string, 2 config_path: string, 3 keymaps: repeated string,
        4 search: KbSearchView, 5 context_filter: string, 6 context_filtered: bool,
        7 source_filter: string, 8 source_filtered: bool, 9 count: string,
        10 has_changes: bool, 11 rows: repeated KbRow, 12 selected: uint64,
        13 scroll_offset: uint32, 14 viewport: uint32, 15 showing_help: bool,
        16 edit_dialog: maybe KbEditDialog, 17 confirm: maybe KbConfirm,
    }
    MapEntryView { 1 key: string, 2 display: string }
    SettingItemView {
        1 index: uint64, 2 path: string, 3 name: string, 4 description: optional string,
        5 section: optional string, 6 section_start: bool, 7 modified: bool, 8 read_only: bool,
        9 nullable: bool, 10 is_null: bool, 11 selected: bool, 12 control: SettingControl,
    }
    SettingsCategoryView {
        1 index: uint64, 2 name: string, 3 selected: bool, 4 expandable: bool,
        5 expanded: bool, 6 sections: repeated string, 7 nested: bool,
    }
    SettingsSearchResultView { 1 name: string, 2 category: string }
    EntryDialogView {
        1 title: string, 2 is_new: bool, 3 items: repeated SettingItemView,
        4 selected_item: uint64, 5 focus_on_buttons: bool, 6 focused_button: uint64,
        7 no_delete: bool,
    }
    SettingsView {
        1 title: string, 2 focus: string, 3 target_layer: string,
        4 categories: repeated SettingsCategoryView, 5 items: repeated SettingItemView,
        6 footer_buttons: repeated string, 7 footer_selected: uint64, 8 search_active: bool,
        9 search_query: string, 10 search_results: repeated SettingsSearchResultView,
        11 search_selected: uint64, 12 entry_dialog: maybe EntryDialogView,
        13 showing_help: bool, 14 showing_confirm: bool, 15 showing_reset: bool,
    }
    /// A plugin's mark on a pane's scrollbar: its row on the track, 0xRRGGBB.
    ScrollbarMarker { 1 row: uint32, 2 color: uint32 }
    /// A pane: where its content and scrollbar are, its tab bar, the thumb.
    PaneModel as Pane {
        1 leaf: uint64, 2 buffer: optional uint64, 3 content: Rect, 4 gutter_width: uint32,
        5 tab_bar: TabBarView, 6 vscroll: Rect, 7 thumb_start: uint64, 8 thumb_end: uint64,
        9 markers: repeated ScrollbarMarker, 10 compose: maybe ComposePane,
    }
    /// A source-preserving rich Compose window. All offsets are UTF-8 bytes;
    /// display offsets are relative to text, source offsets are buffer-wide.
    ComposePane {
        1 buffer_revision: uint64, 2 projection_revision: uint64,
        3 source_start: uint64, 4 source_end: uint64, 5 text: string,
        6 segments: repeated ComposeSegment, 7 spans: repeated ComposeSpan,
        8 images: repeated ComposeImage, 9 cursors: repeated ComposeSelection,
        10 focused: bool, 11 top_byte: uint64, 12 document_len: uint64,
        13 selection_revision: uint64, 14 keyboard: bool, 15 asset_authority: uint64, 16 asset_document: uint64,
        17 viewport: maybe ComposeViewport,
    }
    /// kind: 0 copied text, 1 hidden source, 2 atomic replacement, 3 insertion.
    ComposeSegment {
        1 source_start: uint64, 2 source_end: uint64,
        3 display_start: uint64, 4 display_end: uint64, 5 kind: uint32,
    }
    /// role: body 0, headings 1-6, code 7, table 8, quote 9.
    /// flags: bold 1, italic 2, underline 4, strikethrough 8.
    ComposeSpan { 1 start: uint64, 2 end: uint64, 3 role: uint32, 4 flags: uint32 }
    ComposeImage {
        1 start: uint64, 2 end: uint64, 3 target: string,
        4 alt: string, 5 title: string, 6 block: bool, 7 rendered: bool,
    }
    ComposeSelection { 1 position: uint64, 2 anchor: optional uint64, 3 id: uint64, 4 primary: bool }
    /// A revisioned input was applied exactly once, requires a fresh layout,
    /// or was cancelled with a terminal explanation. Cancelled ids are consumed.
    InputReply {
        1 id: uint64, 2 accepted: bool, 3 needs_layout: bool,
        4 need_region: optional uint64, 5 carets: repeated ComposeCaret,
        6 need_y: optional double, 7 compose: maybe ComposePane,
        8 cancelled: bool, 9 error: string,
    }
    CaretState as ComposeCaret { 1 id: uint64, 2 position: uint64, 3 affinity: uint32 }
    /// A line between panes.
    SeparatorModel as Separator { 1 vertical: bool, 2 x: uint32, 3 y: uint32, 4 len: uint32 }
    /// The colors chrome is drawn with, 0xRRGGBB, each left out for the default.
    SceneTheme as Theme {
        1 name: string, 2 bg: optional uint32, 3 fg: optional uint32,
        4 accent: optional uint32, 5 muted: optional uint32, 6 selection_bg: optional uint32,
        7 menu_bg: optional uint32, 8 menu_fg: optional uint32, 9 menu_hi: optional uint32,
        10 menu_highlight_fg: optional uint32, 11 popup_bg: optional uint32,
        12 popup_fg: optional uint32, 13 border: optional uint32, 14 status_bg: optional uint32,
        15 status_fg: optional uint32, 16 tab_active_bg: optional uint32,
    }
    /// A frame's timing (telemetry on): which of the client's messages it
    /// shows, where building it went, what waited ahead of it, Fresh's CPU
    /// time, and the previous push's allocations (an alloc-stats build).
    Timing {
        1 ack: uint64, 2 build_ms: double, 3 render_ms: double, 4 grid_ms: double,
        5 encode_ms: double, 6 queued: uint64, 7 ahead: uint64, 8 owed: uint32, 9 top: uint64,
        10 cpu_ms: double, 11 prev_allocs: optional uint64, 12 prev_alloc_bytes: optional uint64,
    }
}

proto_oneof! {
    /// A menu item: an action, a separator, a submenu or a label. An action's
    /// arguments are the keymap's JSON, handed back untouched with the action.
    MenuItemView as MenuItem, item {
        1 action: Action as MenuAction {
            1 label: string, 2 action: string, 3 args as args_json: json, 4 accel: optional string,
            5 enabled: bool, 6 checked: optional bool,
        },
        2 separator: Sep as MenuSeparator {},
        3 submenu: Submenu as MenuSubmenu { 1 label: string, 2 items: repeated MenuItem },
        4 label: Label as MenuLabel { 1 label: string },
    }
    /// A popup's content: a list to pick from, or styled lines.
    PopupContentView as PopupContent, content {
        1 list: List as PopupList { 1 items: repeated PopupItemView, 2 selected: uint64 },
        2 lines: Lines as PopupLines { 1 lines: repeated PopupLine },
    }
    /// A row of the keybinding editor: a section header or a binding.
    KbRow as KbRow, row {
        1 section: Section as KbSection {
            1 name: string, 2 collapsed: bool, 3 count: uint64, 4 selected: bool,
        },
        2 binding: Binding as KbBinding {
            1 key: string, 2 action: string, 3 description: string, 4 context: string,
            5 source: string, 6 selected: bool,
        },
    }
    /// A setting's control.
    SettingControlView as SettingControl, control {
        1 toggle: Toggle as SettingToggle { 1 checked: bool },
        2 number: Number as SettingNumber {
            1 value: double, 2 min: optional double, 3 max: optional double, 4 integer: bool,
            5 percent: bool,
        },
        3 dropdown: Dropdown as SettingDropdown {
            1 selected: uint64, 2 options: repeated string, 3 open: bool,
        },
        4 text: Text as SettingText { 1 value: string, 2 editing: bool, 3 placeholder: string },
        5 text_list: TextList as SettingTextList { 1 items: repeated string, 2 focused: optional uint64 },
        6 dual_list: DualList as SettingDualList {
            1 included: repeated string, 2 available: repeated string,
            3 included_cursor: uint64, 4 available_cursor: uint64, 5 active_column: string,
        },
        7 map: Map as SettingMap {
            1 entries: repeated MapEntryView, 2 column: optional string, 3 no_add: bool,
            4 focused: optional uint64, 5 add_focused: bool,
        },
        8 object_array: ObjectArray as SettingObjectArray {
            1 entries: repeated string, 2 focused: optional uint64, 3 add_focused: bool,
        },
        9 json: Json as SettingJson { 1 value: string },
        10 complex: Complex as SettingComplex { 1 type_name: string },
    }
}

/// The model's rectangles (ratatui's) go out as `Rect` too.
impl Message for ratatui::layout::Rect {
    fn encoded_len(&self) -> usize {
        field::uint_len(1, u64::from(self.x))
            + field::uint_len(2, u64::from(self.y))
            + field::uint_len(3, u64::from(self.width))
            + field::uint_len(4, u64::from(self.height))
    }
    fn encode(&self, buf: &mut Vec<u8>) {
        field::uint(buf, 1, u64::from(self.x));
        field::uint(buf, 2, u64::from(self.y));
        field::uint(buf, 3, u64::from(self.width));
        field::uint(buf, 4, u64::from(self.height));
    }
}

/// One line of a popup: its spans.
impl Message for Vec<PopupSpanView> {
    fn encoded_len(&self) -> usize {
        field::msgs_len(1, self.iter())
    }
    fn encode(&self, buf: &mut Vec<u8>) {
        field::msgs(buf, 1, self.iter())
    }
}

/// The clipboard's latest copy: its number, which rises with every new copy,
/// and its text.
pub(crate) struct Clipboard<'a> {
    pub(crate) seq: u64,
    pub(crate) text: &'a str,
}

impl Message for Clipboard<'_> {
    fn encoded_len(&self) -> usize {
        field::uint_len(1, self.seq) + field::text_len(2, self.text)
    }
    fn encode(&self, buf: &mut Vec<u8>) {
        field::uint(buf, 1, self.seq);
        field::text(buf, 2, self.text);
    }
}

/// The cell the hardware cursor is on.
pub(crate) struct Cursor {
    pub(crate) x: u16,
    pub(crate) y: u16,
}

impl Message for Cursor {
    fn encoded_len(&self) -> usize {
        field::uint_len(1, u64::from(self.x)) + field::uint_len(2, u64::from(self.y))
    }
    fn encode(&self, buf: &mut Vec<u8>) {
        field::uint(buf, 1, u64::from(self.x));
        field::uint(buf, 2, u64::from(self.y));
    }
}

/// What the schema holds besides the generated messages: the frame and its
/// parts, the hellos, and the client's messages.
const FRAME_SCHEMA: &str = r#"// One line of a popup: its spans.
message PopupLine {
  repeated PopupSpanView spans = 1;
}
// The clipboard's latest copy; seq rises with every new copy.
message Clipboard {
  uint64 seq = 1;
  string text = 2;
}
// The cell the hardware cursor is on.
message Cursor {
  uint32 x = 1;
  uint32 y = 2;
}
// One row of the grid: its text once, and its runs as byte lengths into that
// text, each with a style: fg | bg << 25 | flags << 50, a color being
// 0x1RRGGBB (0 for the terminal default) and the flags bold 1, italic 2,
// underline 4, reverse 8.
message GridRow {
  uint32 index = 1;
  string text = 2;
  repeated uint32 run_bytes = 3;
  repeated uint64 run_styles = 4;
}
message PaneUpdate {
  uint32 index = 1;
  Pane pane = 2;
}
// A region's update: present when it changed; without its value when it is
// gone.
message MenusUpdate {
  repeated MenuEntry menus = 1;
}
message StatusBarUpdate {
  StatusView value = 1;
}
message FileExplorerUpdate {
  FileExplorerView value = 1;
}
message SeparatorsUpdate {
  repeated Separator separators = 1;
}
message PopupsUpdate {
  repeated ScenePopup popups = 1;
}
message PaletteUpdate {
  PaletteView value = 1;
}
message TrustDialogUpdate {
  TrustDialogView value = 1;
}
message ContextMenuUpdate {
  ContextMenuView value = 1;
}
message KeybindingEditorUpdate {
  KeybindingEditorView value = 1;
}
message SettingsUpdate {
  SettingsView value = 1;
}
// What changed since the last frame the client got; a ServerHello's frame
// carries everything. Copies name grid rows the client already has, as
// [to, from, count] triples: rows to..to+count become its previous rows
// from..from+count, applied before the rows sent in full.
message Frame {
  uint64 seq = 1;
  optional uint32 width = 2;
  optional uint32 height = 3;
  optional uint64 window_id = 4;
  Theme theme = 5;
  Clipboard clipboard = 6;
  Cursor cursor = 7;
  optional uint32 grid_len = 8;
  repeated uint32 copies = 9;
  repeated GridRow rows = 10;
  optional uint32 pane_len = 11;
  repeated PaneUpdate panes = 12;
  MenusUpdate menus = 13;
  StatusBarUpdate status_bar = 14;
  FileExplorerUpdate file_explorer = 15;
  SeparatorsUpdate separators = 16;
  PopupsUpdate popups = 17;
  PaletteUpdate palette = 18;
  TrustDialogUpdate trust_dialog = 19;
  ContextMenuUpdate context_menu = 20;
  KeybindingEditorUpdate keybinding_editor = 21;
  SettingsUpdate settings = 22;
  Timing timing = 23;
  InputReply input_reply = 24;
  repeated ComposeAssetChunk compose_assets = 25;
  ComposeViewportReply viewport_reply = 26;
}
// Type 1: the answer to a ClientHello, with the whole scene.
message ServerHello {
  uint32 version = 1;
  Frame scene = 2;
}
// Type 3: why Fresh is closing the connection.
message Error {
  string reason = 1;
}
// Type 16: the client's first message. Telemetry asks for frame timing.
message ClientHello {
  uint32 version = 1;
  bool telemetry = 2;
  // Opt in to source-mapped Compose panes. Grid cells remain available.
  bool rich_compose = 3;
}
// A pane identity and the exact source/projection the client interacted with.
message ComposeScope {
  uint64 window_id = 1;
  uint64 leaf = 2;
  uint64 buffer = 3;
  uint64 buffer_revision = 4;
  uint64 projection_revision = 5;
  uint64 selection_revision = 6;
}
// Type 27: serialized input for a rich client. kind/payload carry an existing
// input message unchanged. A pointer instead supplies a source position.
// Fresh acknowledges even a no-op. Nonterminal rejections are retried after
// the frame. A source_revision pointer is rebased from event-time coordinates;
// buffer_epoch prevents reusing them after buffer replacement. cancel consumes
// this id and clears its deferred action without executing it.
message ComposeInput {
  uint64 id = 1;
  ComposeScope scope = 2;
  uint32 kind = 3;
  bytes payload = 4;
  optional uint64 position = 5;
  bool extend = 6;
  uint32 click_count = 7;
  ComposeLayout layout = 8;
  uint64 layout_id = 9;
  uint32 affinity = 10;
  optional uint64 region = 11;
  optional uint64 attachment_start = 12;
  optional uint64 source_revision = 13;
  uint64 buffer_epoch = 14;
  bool cancel = 15;
}
// Type 28: read the image annotation identified by this source range through
// its document authority. No client-supplied path. Cancellation needs only id.
message ComposeAssetRequest {
  uint64 id = 1;
  ComposeScope scope = 2;
  uint64 start = 3;
  uint64 end = 4;
  bool cancel = 5;
  uint64 authority = 6;
  uint64 document = 7;
}
// Type 29: coalesced pixel scrolling. The source marker is remapped from the
// scope's buffer revision. offset is relative to the native content top.
message ComposeViewportRequest {
  uint64 id = 1;
  ComposeScope scope = 2;
  uint64 position = 3;
  uint32 affinity = 4;
  double offset = 5;
  uint64 epoch = 6;
}
// Measured geometry belongs to this client. Reused by id until source,
// projection, width or fonts change. All coordinates are native points.
message ComposeLayout {
  uint64 id = 1;
  double page_height = 2;
  uint64 source_start = 3;
  uint64 source_end = 4;
  uint64 document_len = 5;
  repeated ComposeLine lines = 6;
  uint64 total_lines = 7;
  double height = 8;
  uint64 style_revision = 9;
}
message ComposeLine {
  double y = 1;
  double height = 2;
  uint64 source_start = 3;
  uint64 source_end = 4;
  repeated ComposeStop stops = 5;
  ComposeStop start = 6;
  ComposeStop end = 7;
  uint64 number = 8;
}
message ComposeStop {
  uint64 source = 1;
  double x = 2;
  uint32 affinity = 3;
}
// Type 17: a key, as a browser names it (key), the physical key (code), and
// the modifiers held.
message Key {
  string key = 1;
  string code = 2;
  bool ctrl = 3;
  bool alt = 4;
  bool meta = 5;
  bool shift = 6;
}
enum MouseKind {
  MOUSE_KIND_UNKNOWN = 0;
  MOUSE_KIND_DOWN = 1;
  MOUSE_KIND_UP = 2;
  MOUSE_KIND_DRAG = 3;
  MOUSE_KIND_MOVED = 4;
  MOUSE_KIND_SCROLL_UP = 5;
  MOUSE_KIND_SCROLL_DOWN = 6;
  MOUSE_KIND_SCROLL_LEFT = 7;
  MOUSE_KIND_SCROLL_RIGHT = 8;
}
enum MouseButton {
  MOUSE_BUTTON_LEFT = 0;
  MOUSE_BUTTON_RIGHT = 1;
  MOUSE_BUTTON_MIDDLE = 2;
}
// Type 18: a mouse event on a cell, n times (wheel notches), with the click
// count the client counted.
message Mouse {
  MouseKind kind = 1;
  MouseButton button = 2;
  uint32 col = 3;
  uint32 row = 4;
  uint32 n = 5;
  uint64 count = 6;
  bool ctrl = 7;
  bool alt = 8;
  bool shift = 9;
}
// Type 19: the grid size the client wants.
message Resize {
  uint32 cols = 1;
  uint32 rows = 2;
}
// Type 20: text to paste.
message Paste {
  string text = 1;
}
// Type 21: an action by name, with the arguments a menu item gave it.
message Action {
  string name = 1;
  string args_json = 2;
}
// Type 22: a click on a widget of a surface (the palette's toolbar).
message Widget {
  string surface = 1;
  optional string key = 2;
}
// Type 23: a click in the settings modal: what was hit (kind), its indices,
// a double click, an entry dialog's button.
message SettingsClick {
  string kind = 1;
  uint64 a = 2;
  uint64 b = 3;
  bool double = 4;
  string button = 5;
}
// Type 24: a click on a row of the keybinding editor.
message KbEditClick {
  optional uint64 row = 1;
}
// Type 25: frame timing on or off.
message Configure {
  bool telemetry = 1;
}
// Type 26: the last frame the client applied.
message Applied {
  uint64 seq = 1;
}
"#;

/// `scene.proto`, as the encoders here write it.
pub(crate) fn schema() -> String {
    let mut out = String::from(
        "// The scene channel of File Commander's sessions: protobuf messages in a\n\
         // type-length-value stream (type and length as varints) over the session's\n\
         // Unix socket. Generated from crates/fresh-editor/src/webui/wire.rs, which\n\
         // writes these messages; a test keeps this file equal to it.\n\
         syntax = \"proto3\";\n\
         package fresh.scene.v1;\n\
         // Swift's generated types are WireFrame, WireRect, ...: Error, Frame\n\
         // and Key would clash with names the app already has.\n\
         option swift_prefix = \"Wire\";\n",
    );
    for part in SCHEMA.iter().chain(ONEOF_SCHEMA).chain(std::iter::once(&FRAME_SCHEMA)) {
        out.push('\n');
        out.push_str(part);
    }
    out
}

// ─────────────────────────── frames ───────────────────────────

use super::{GridFrame, SceneModel};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Position;
use ratatui::style::Modifier;

/// The regions a frame carries, as their field numbers in `Frame`.
const REGION_TAGS: [u32; 10] = [13, 14, 15, 16, 17, 18, 19, 20, 21, 22];

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

/// One push's scene, encoded once for every client: each region's update,
/// each pane, the theme and the clipboard, with a hash of each, and the
/// scalars. The buffers are kept from push to push.
#[derive(Default)]
pub(crate) struct EncodedScene {
    regions: [Vec<u8>; 10],
    region_hashes: [u64; 10],
    panes: Vec<Vec<u8>>,
    pane_hashes: Vec<u64>,
    theme: Vec<u8>,
    theme_hash: u64,
    clipboard: Vec<u8>,
    clipboard_seq: u64,
    width: u16,
    height: u16,
    window_id: u64,
    cursor: (u16, u16),
}

impl EncodedScene {
    pub(crate) fn encode(&mut self, model: &SceneModel<'_>, clipboard: Clipboard<'_>) {
        let m = model;
        self.region(0, |b| field::msgs(b, 1, m.menu.menus.iter()));
        self.region(1, |b| field::opt_msg(b, 1, m.status.as_ref()));
        self.region(2, |b| field::opt_msg(b, 1, m.file_explorer.as_ref()));
        self.region(3, |b| field::msgs(b, 1, m.separators.iter()));
        self.region(4, |b| field::msgs(b, 1, m.popups.iter()));
        self.region(5, |b| field::opt_msg(b, 1, m.palette.as_ref()));
        self.region(6, |b| field::opt_msg(b, 1, m.trust_dialog.as_ref()));
        self.region(7, |b| field::opt_msg(b, 1, m.context_menu.as_ref()));
        self.region(8, |b| field::opt_msg(b, 1, m.keybinding_editor.as_ref()));
        self.region(9, |b| field::opt_msg(b, 1, m.settings.as_ref()));
        if self.panes.len() != m.panes.len() {
            self.panes.resize_with(m.panes.len(), Vec::new);
            self.pane_hashes.resize(m.panes.len(), 0);
        }
        for (i, pane) in m.panes.iter().enumerate() {
            let b = &mut self.panes[i];
            b.clear();
            pane.encode(b);
            self.pane_hashes[i] = hash_bytes(b);
        }
        self.theme.clear();
        m.theme.encode(&mut self.theme);
        self.theme_hash = hash_bytes(&self.theme);
        self.clipboard.clear();
        clipboard.encode(&mut self.clipboard);
        self.clipboard_seq = clipboard.seq;
        self.width = m.buf.area.width;
        self.height = m.buf.area.height;
        self.window_id = m.window_id;
        self.cursor = m.cursor.unwrap_or((0, 0));
    }

    fn region(&mut self, i: usize, write: impl FnOnce(&mut Vec<u8>)) {
        let b = &mut self.regions[i];
        b.clear();
        write(b);
        self.region_hashes[i] = hash_bytes(b);
    }
}

/// A grid row's text and runs, kept between rows.
#[derive(Default)]
struct RowScratch {
    text: Vec<u8>,
    run_bytes: Vec<u32>,
    run_styles: Vec<u64>,
}

/// A cell's style as a `GridRow` run carries it: fg | bg << 25 | flags << 50,
/// a color being 0x1RRGGBB (0 for the terminal default, through the web
/// palette as the JSON grid has it), the flags bold 1, italic 2,
/// underline 4, reverse 8.
fn cell_style(cell: &Cell) -> u64 {
    let color = |c| super::color_rgb(c).map_or(0, |rgb| 1 << 24 | u64::from(rgb.packed()));
    let m = cell.modifier;
    let flags = u64::from(m.contains(Modifier::BOLD))
        | u64::from(m.contains(Modifier::ITALIC)) << 1
        | u64::from(m.contains(Modifier::UNDERLINED)) << 2
        | u64::from(m.contains(Modifier::REVERSED)) << 3;
    color(cell.fg) | color(cell.bg) << 25 | flags << 50
}

impl RowScratch {
    /// Write row `row` of `buf` as a `GridRow` in field `tag`.
    fn write(&mut self, out: &mut Vec<u8>, tag: u32, buf: &Buffer, row: usize) {
        self.text.clear();
        self.run_bytes.clear();
        self.run_styles.clear();
        let area = buf.area;
        let y = area.y + row as u16;
        let mut style = None;
        let mut run_start = 0;
        for x in area.x..area.x.saturating_add(area.width) {
            let Some(cell) = buf.cell(Position::new(x, y)) else {
                continue;
            };
            let s = cell_style(cell);
            if style != Some(s) {
                if let Some(previous) = style {
                    self.run_bytes.push((self.text.len() - run_start) as u32);
                    self.run_styles.push(previous);
                }
                style = Some(s);
                run_start = self.text.len();
            }
            self.text.extend_from_slice(cell.symbol().as_bytes());
        }
        if let Some(previous) = style {
            self.run_bytes.push((self.text.len() - run_start) as u32);
            self.run_styles.push(previous);
        }
        let runs = || self.run_bytes.iter().map(|&n| u64::from(n));
        let styles = || self.run_styles.iter().copied();
        let len = field::uint_len(1, row as u64)
            + if self.text.is_empty() { 0 } else { field::bytes_len(2, self.text.len()) }
            + field::packed_len(3, runs())
            + field::packed_len(4, styles());
        put_key(out, tag, LEN);
        put_varint(out, len as u64);
        field::uint(out, 1, row as u64);
        if !self.text.is_empty() {
            field::raw_bytes(out, 2, &self.text);
        }
        field::packed(out, 3, runs());
        field::packed(out, 4, styles());
    }
}

/// The grid rows that changed but that the client already has at another
/// index of its previous frame, as `[to, from, count]` runs: rows
/// `to..to+count` become its previous rows `from..from+count`. A scroll moves
/// rows rather than changing them, so this names nearly all of them in one
/// run, and only the rows that came into view (and any whose content changed,
/// such as a scrollbar's) are sent in full. Into buffers kept between frames.
pub(crate) fn grid_copies_into(
    old: &[u64],
    new: &[u64],
    at: &mut HashMap<u64, usize>,
    runs: &mut Vec<[usize; 3]>,
) {
    at.clear();
    runs.clear();
    for (i, h) in old.iter().enumerate() {
        at.entry(*h).or_insert(i);
    }
    for (i, h) in new.iter().enumerate() {
        if old.get(i) == Some(h) {
            continue;
        }
        // Rows moved together stay together: extend the run before this row.
        if let Some(run) = runs.last_mut() {
            if run[0] + run[2] == i && old.get(run[1] + run[2]) == Some(h) {
                run[2] += 1;
                continue;
            }
        }
        if let Some(&from) = at.get(h) {
            runs.push([i, from, 1]);
        }
    }
}

/// What a client has of the scene, so the next frame brings it up to date
/// with only what changed, and the buffers that frame is worked out in.
#[derive(Default)]
pub(crate) struct ClientScene {
    size: Option<(u16, u16)>,
    window_id: Option<u64>,
    cursor: Option<(u16, u16)>,
    clipboard_seq: Option<u64>,
    theme: Option<u64>,
    regions: [Option<u64>; 10],
    panes: Vec<u64>,
    grid: Vec<u64>,
    copies_at: HashMap<u64, usize>,
    copies: Vec<[usize; 3]>,
    row: RowScratch,
}

impl ClientScene {
    /// Write into `out` the fields of the `Frame` that brings this client up
    /// to `scene` and `grid`. True when it says anything besides its number
    /// and timing.
    pub(crate) fn write_frame(
        &mut self,
        out: &mut Vec<u8>,
        seq: u64,
        scene: &EncodedScene,
        grid: Option<&GridFrame<'_>>,
        timing: Option<&Timing>,
    ) -> bool {
        field::uint(out, 1, seq);
        let mut changed = false;
        let size = (scene.width, scene.height);
        if self.size != Some(size) {
            field::opt_uint(out, 2, Some(u64::from(size.0)));
            field::opt_uint(out, 3, Some(u64::from(size.1)));
            self.size = Some(size);
            changed = true;
        }
        if self.window_id != Some(scene.window_id) {
            field::opt_uint(out, 4, Some(scene.window_id));
            self.window_id = Some(scene.window_id);
            changed = true;
        }
        if self.theme != Some(scene.theme_hash) {
            field::raw_bytes(out, 5, &scene.theme);
            self.theme = Some(scene.theme_hash);
            changed = true;
        }
        if self.clipboard_seq != Some(scene.clipboard_seq) {
            field::raw_bytes(out, 6, &scene.clipboard);
            self.clipboard_seq = Some(scene.clipboard_seq);
            changed = true;
        }
        if self.cursor != Some(scene.cursor) {
            let (x, y) = scene.cursor;
            field::msg(out, 7, &Cursor { x, y });
            self.cursor = Some(scene.cursor);
            changed = true;
        }
        if let Some(grid) = grid {
            changed |= self.write_grid(out, grid);
        }
        for (i, h) in scene.pane_hashes.iter().enumerate() {
            if self.panes.get(i) == Some(h) {
                continue;
            }
            let pane = &scene.panes[i];
            let len = field::uint_len(1, i as u64) + field::bytes_len(2, pane.len());
            put_key(out, 12, LEN);
            put_varint(out, len as u64);
            field::uint(out, 1, i as u64);
            field::raw_bytes(out, 2, pane);
            changed = true;
        }
        if scene.pane_hashes.len() != self.panes.len() {
            field::opt_uint(out, 11, Some(scene.pane_hashes.len() as u64));
            changed = true;
        }
        self.panes.clear();
        self.panes.extend_from_slice(&scene.pane_hashes);
        for (i, tag) in REGION_TAGS.iter().enumerate() {
            if self.regions[i] != Some(scene.region_hashes[i]) {
                field::raw_bytes(out, *tag, &scene.regions[i]);
                self.regions[i] = Some(scene.region_hashes[i]);
                changed = true;
            }
        }
        if let Some(timing) = timing {
            field::msg(out, 23, timing);
        }
        changed
    }

    fn write_grid(&mut self, out: &mut Vec<u8>, grid: &GridFrame<'_>) -> bool {
        let new = grid.hashes;
        grid_copies_into(&self.grid, new, &mut self.copies_at, &mut self.copies);
        let mut changed = !self.copies.is_empty();
        field::packed(out, 9, self.copies.iter().flat_map(|run| run.iter().map(|&v| v as u64)));
        for (i, h) in new.iter().enumerate() {
            let copied = self.copies.iter().any(|&[to, _, n]| (to..to + n).contains(&i));
            if self.grid.get(i) == Some(h) || copied {
                continue;
            }
            self.row.write(out, 10, grid.buf, i);
            changed = true;
        }
        if new.len() != self.grid.len() {
            field::opt_uint(out, 8, Some(new.len() as u64));
            changed = true;
        }
        self.grid.clear();
        self.grid.extend_from_slice(new);
        changed
    }

    /// Write into `out` the fields of a `ServerHello`: the whole scene, which
    /// this client (new) then has.
    pub(crate) fn write_hello(
        &mut self,
        out: &mut Vec<u8>,
        scene: &EncodedScene,
        grid: Option<&GridFrame<'_>>,
        timing: Option<&Timing>,
    ) {
        let mut frame = Vec::new();
        self.write_frame(&mut frame, 0, scene, grid, timing);
        field::uint(out, 1, u64::from(VERSION));
        field::raw_bytes(out, 2, &frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/webui/scene.proto")
    }

    fn copies(old: &[u64], new: &[u64]) -> Vec<[usize; 3]> {
        let mut runs = Vec::new();
        grid_copies_into(old, new, &mut std::collections::HashMap::new(), &mut runs);
        runs
    }

    #[test]
    fn rows_that_moved_are_named_by_where_they_were() {
        // Lines 1..6 scrolled by two: four rows moved up, two came into view.
        assert_eq!(copies(&[1, 2, 3, 4, 5, 6], &[3, 4, 5, 6, 7, 8]), vec![[0, 2, 4]]);
        // A row that changed (a scrollbar's) splits the run; rows that stayed
        // where they were need nothing.
        assert_eq!(copies(&[1, 2, 3, 4, 5, 6], &[3, 4, 9, 6, 5, 6]), vec![[0, 2, 2], [3, 5, 1]]);
        assert!(copies(&[1, 2], &[1, 2]).is_empty());
    }

    /// `scene.proto` is what the encoders write. `UPDATE_SCENE_PROTO=1`
    /// writes it again after a message changed.
    #[test]
    fn scene_proto_matches_the_encoders() {
        let schema = schema();
        if std::env::var_os("UPDATE_SCENE_PROTO").is_some() {
            std::fs::write(proto_path(), &schema).expect("write scene.proto");
        }
        let file = std::fs::read_to_string(proto_path()).unwrap_or_default();
        assert!(
            file == schema,
            "scene.proto differs from the encoders; run with UPDATE_SCENE_PROTO=1 and regenerate"
        );
    }

    #[test]
    fn varints_round_trip_and_bad_ones_are_refused() {
        for v in [0u64, 1, 127, 128, 300, 16_383, 16_384, u64::from(u32::MAX), u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(buf.len(), varint_len(v), "{v}");
            assert_eq!(read_varint(&buf), Ok(Some((v, buf.len()))), "{v}");
            // Every prefix is incomplete, not wrong.
            for n in 0..buf.len() {
                assert_eq!(read_varint(&buf[..n]), Ok(None), "{v} cut at {n}");
            }
        }
        assert_eq!(read_varint(&[0x80, 0x00]), Err(WireError::NotMinimal));
        assert_eq!(read_varint(&[0xff; 10]), Err(WireError::LongVarint));
        assert_eq!(
            read_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]),
            Err(WireError::LongVarint)
        );
    }

    #[test]
    fn messages_are_framed_by_type_and_length() {
        let mut buf = Vec::new();
        put_header(&mut buf, kind::FRAME, 3);
        buf.extend_from_slice(b"abc");
        put_header(&mut buf, kind::APPLIED, 0);
        let (k, payload, used) = read_message(&buf).unwrap().unwrap();
        assert_eq!((k, &buf[payload], used), (kind::FRAME, &b"abc"[..], 5));
        assert_eq!(read_message(&buf[used..]).unwrap(), Some((kind::APPLIED, 2..2, 2)));
        assert_eq!(read_message(&buf[..4]).unwrap(), None, "a payload still arriving");
        let mut huge = Vec::new();
        put_header(&mut huge, kind::FRAME, MAX_PAYLOAD + 1);
        assert!(matches!(read_message(&huge), Err(WireError::TooLong(_))));
    }
}
