use std::io::{Error, ErrorKind, Write};

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const MAX_PACKET_LEN: i32 = 2_097_152;

#[derive(Debug, Clone, Default)]
pub struct ByteBuffer {
    inner: BytesMut,
}

impl ByteBuffer {
    pub fn new() -> Self {
        Self {
            inner: BytesMut::new(),
        }
    }

    pub fn push(&mut self, byte: u8) {
        self.inner.extend_from_slice(&[byte]);
    }

    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.inner.extend_from_slice(bytes);
    }

    pub fn as_slice(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl Write for ByteBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn write_varint_buffer(buf: &mut ByteBuffer, value: i32) {
    let mut val = value as u32;
    loop {
        if (val & !0x7F) == 0 {
            buf.push(val as u8);
            break;
        }
        buf.push(((val & 0x7F) as u8) | 0x80);
        val >>= 7;
    }
}

pub fn write_varint(buf: &mut Vec<u8>, value: i32) {
    let mut val = value as u32;
    loop {
        if (val & !0x7F) == 0 {
            buf.push(val as u8);
            break;
        }
        buf.push(((val & 0x7F) as u8) | 0x80);
        val >>= 7;
    }
}

pub fn read_varint_from_slice(data: &[u8], offset: &mut usize) -> std::io::Result<i32> {
    let mut num_read = 0;
    let mut result: i32 = 0;

    loop {
        if *offset >= data.len() {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "unexpected eof while reading varint",
            ));
        }

        let read = data[*offset];
        *offset += 1;

        let value = (read & 0x7F) as i32;
        result |= value << (7 * num_read);

        num_read += 1;
        if num_read > 5 {
            return Err(Error::new(ErrorKind::InvalidData, "varint too big"));
        }

        if (read & 0x80) == 0 {
            break;
        }
    }

    Ok(result)
}

pub async fn read_varint(stream: &mut TcpStream) -> std::io::Result<i32> {
    let mut num_read = 0;
    let mut result: i32 = 0;

    loop {
        let mut one = [0u8; 1];
        stream.read_exact(&mut one).await?;

        let value = (one[0] & 0x7F) as i32;
        result |= value << (7 * num_read);

        num_read += 1;
        if num_read > 5 {
            return Err(Error::new(ErrorKind::InvalidData, "varint too big"));
        }

        if (one[0] & 0x80) == 0 {
            break;
        }
    }

    Ok(result)
}

pub fn read_string_from_slice(data: &[u8], offset: &mut usize) -> std::io::Result<String> {
    let len = read_varint_from_slice(data, offset)?;
    if len < 0 {
        return Err(Error::new(ErrorKind::InvalidData, "negative string length"));
    }

    let len = len as usize;
    if *offset + len > data.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "not enough bytes for string",
        ));
    }

    let value = std::str::from_utf8(&data[*offset..*offset + len])
        .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid utf8 in string"))?
        .to_owned();

    *offset += len;
    Ok(value)
}

pub fn read_bool_from_slice(data: &[u8], offset: &mut usize) -> std::io::Result<bool> {
    if *offset >= data.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "unexpected eof while reading bool",
        ));
    }

    let value = data[*offset];
    *offset += 1;

    match value {
        0x00 => Ok(false),
        0x01 => Ok(true),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            format!("invalid boolean byte 0x{value:02x}"),
        )),
    }
}

pub fn write_string(buf: &mut Vec<u8>, value: &str) -> std::io::Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() > i32::MAX as usize {
        return Err(Error::new(ErrorKind::InvalidInput, "string too long"));
    }

    write_varint(buf, bytes.len() as i32);
    buf.extend_from_slice(bytes);
    Ok(())
}

pub async fn read_packet(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let packet_len = read_varint(stream).await?;
    if !(0..=MAX_PACKET_LEN).contains(&packet_len) {
        return Err(Error::new(ErrorKind::InvalidData, "invalid packet length"));
    }

    let mut payload = vec![0u8; packet_len as usize];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

pub async fn write_packet(
    stream: &mut TcpStream,
    packet_id: i32,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut packet = Vec::with_capacity(8 + payload.len());
    write_varint(&mut packet, packet_id);
    packet.extend_from_slice(payload);
    write_framed_payload(stream, &packet).await
}

pub async fn write_framed_payload(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_PACKET_LEN as usize {
        return Err(Error::new(ErrorKind::InvalidInput, "payload too large"));
    }

    let mut frame = ByteBuffer::new();
    write_varint_buffer(&mut frame, payload.len() as i32);
    frame.extend_from_slice(payload);
    stream.write_all(frame.as_slice()).await?;
    Ok(())
}

pub fn packet_id(packet: &[u8]) -> std::io::Result<i32> {
    let mut offset = 0usize;
    read_varint_from_slice(packet, &mut offset)
}

pub fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}
