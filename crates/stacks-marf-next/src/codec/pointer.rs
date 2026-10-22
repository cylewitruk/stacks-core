// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Pointer widths, flags, and checked byte reads.
use super::{Bad, COMPRESSED, INLINE_ANNOTATION, Reader, WIDE_PTR, compressed_has_block, wide};
use crate::node::Ptr;

pub fn ptr_plain_len(ptr: &Ptr) -> usize {
    1 + 1 + if wide(ptr.ptr) { 8 } else { 4 } + 4
}

pub fn ptr_compressed_len(ptr: &Ptr) -> usize {
    let bb = ptr.is_back() || ptr.back_block != 0;
    1 + 1 + if wide(ptr.ptr) { 8 } else { 4 } + if bb { 4 } else { 0 }
}

pub fn put_ptr_plain(ptr: &Ptr, out: &mut Vec<u8>) {
    let w = wide(ptr.ptr);
    out.push(ptr.id | if w { WIDE_PTR } else { 0 });
    out.push(ptr.chr);
    if w {
        out.extend_from_slice(&ptr.ptr.to_be_bytes());
    } else {
        out.extend_from_slice(&(ptr.ptr as u32).to_be_bytes());
    }
    out.extend_from_slice(&ptr.back_block.to_be_bytes());
}

pub fn put_ptr_compressed(ptr: &Ptr, out: &mut Vec<u8>) {
    let w = wide(ptr.ptr);
    let annotated = !ptr.is_back() && ptr.back_block != 0;
    out.push(
        ptr.id
            | COMPRESSED
            | if w { WIDE_PTR } else { 0 }
            | if annotated { INLINE_ANNOTATION } else { 0 },
    );
    out.push(ptr.chr);
    if w {
        out.extend_from_slice(&ptr.ptr.to_be_bytes());
    } else {
        out.extend_from_slice(&(ptr.ptr as u32).to_be_bytes());
    }
    if ptr.is_back() || annotated {
        out.extend_from_slice(&ptr.back_block.to_be_bytes());
    }
}
impl<'a> Reader<'a> {
    #[inline]
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        let end = self.pos.checked_add(n).ok_or(Bad::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(Bad::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8, Bad> {
        Ok(self.take(1)?[0])
    }

    #[inline]
    pub fn u32(&mut self) -> Result<u32, Bad> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    #[inline]
    pub fn offset(&mut self, id: u8) -> Result<u64, Bad> {
        if id & WIDE_PTR != 0 {
            Ok(u64::from_be_bytes(
                self.take(8)?.try_into().expect("8 bytes"),
            ))
        } else {
            Ok(u64::from(self.u32()?))
        }
    }

    pub fn ptr_plain(&mut self) -> Result<Ptr, Bad> {
        let raw = self.u8()?;
        let chr = self.u8()?;
        let ptr = self.offset(raw)?;
        let back_block = self.u32()?;
        Ok(Ptr {
            id: raw & !WIDE_PTR,
            chr,
            ptr,
            back_block,
        })
    }

    pub fn ptr_compressed(&mut self) -> Result<Ptr, Bad> {
        let raw = self.u8()?;
        let chr = self.u8()?;
        let ptr = self.offset(raw)?;
        let back_block = if compressed_has_block(raw) {
            self.u32()?
        } else {
            0
        };
        Ok(Ptr {
            id: raw & !(COMPRESSED | WIDE_PTR | INLINE_ANNOTATION),
            chr,
            ptr,
            back_block,
        })
    }

    pub fn path_bytes(&mut self) -> Result<&'a [u8], Bad> {
        let len = self.u8()? as usize;
        if len > 32 {
            return Err(Bad::PathTooLong);
        }
        self.take(len)
    }
}
