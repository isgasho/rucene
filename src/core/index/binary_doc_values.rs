// Copyright 2019 Zhizhesihai (Beijing) Technology Limited.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use core::codec::CompressedBinaryTermIterator;
use core::codec::{BinaryEntry, ReverseTermsIndexRef};
use core::index::SeekStatus;
use core::index::TermIterator;
use core::store::IndexInput;
use core::util::packed::MonotonicBlockPackedReaderRef;
use core::util::DocId;
use core::util::LongValues;
use error::Result;

use std::sync::Arc;

pub trait BinaryDocValues: Send + Sync {
    fn get(&self, doc_id: DocId) -> Result<Vec<u8>>;
}

pub type BinaryDocValuesRef = Arc<dyn BinaryDocValues>;

pub struct EmptyBinaryDocValues;

impl BinaryDocValues for EmptyBinaryDocValues {
    fn get(&self, _doc_id: DocId) -> Result<Vec<u8>> {
        Ok(Vec::with_capacity(0))
    }
}

pub trait LongBinaryDocValues: BinaryDocValues {
    fn get64(&self, doc_id: i64) -> Result<Vec<u8>>;
}

pub struct FixedBinaryDocValues {
    data: Box<dyn IndexInput>,
    buffer_len: usize,
}

impl FixedBinaryDocValues {
    pub fn new(data: Box<dyn IndexInput>, buffer_len: usize) -> Self {
        FixedBinaryDocValues { data, buffer_len }
    }
}

impl LongBinaryDocValues for FixedBinaryDocValues {
    fn get64(&self, id: i64) -> Result<Vec<u8>> {
        let length = self.buffer_len;
        let mut data = self.data.as_ref().clone()?;
        data.seek(id * length as i64)?;
        let mut buffer = vec![0u8; length];
        data.read_bytes(&mut buffer, 0, length)?;
        Ok(buffer)
    }
}

impl BinaryDocValues for FixedBinaryDocValues {
    fn get(&self, doc_id: DocId) -> Result<Vec<u8>> {
        FixedBinaryDocValues::get64(self, i64::from(doc_id))
    }
}

pub struct VariableBinaryDocValues<T: LongValues> {
    addresses: T,
    data: Box<dyn IndexInput>,
}

impl<T: LongValues> VariableBinaryDocValues<T> {
    pub fn new(addresses: T, data: Box<dyn IndexInput>, _length: usize) -> Self {
        VariableBinaryDocValues { addresses, data }
    }
}

impl<T: LongValues> LongBinaryDocValues for VariableBinaryDocValues<T> {
    fn get64(&self, id: i64) -> Result<Vec<u8>> {
        let start_address = self.addresses.get64(id)?;
        let end_address = self.addresses.get64(id + 1)?;
        let length = (end_address - start_address) as usize;
        let mut data = self.data.as_ref().clone()?;
        data.seek(start_address)?;
        let mut buffer = vec![0u8; length];
        data.read_bytes(&mut buffer, 0, length)?;
        Ok(buffer)
    }
}

impl<T: LongValues> BinaryDocValues for VariableBinaryDocValues<T> {
    fn get(&self, doc_id: DocId) -> Result<Vec<u8>> {
        VariableBinaryDocValues::get64(self, i64::from(doc_id))
    }
}

pub struct CompressedBinaryDocValues {
    num_values: i64,
    num_index_values: i64,
    num_reverse_index_values: i64,
    max_term_length: i32,
    data: Box<dyn IndexInput>,
    reverse_index: ReverseTermsIndexRef,
    addresses: MonotonicBlockPackedReaderRef,
}

impl CompressedBinaryDocValues {
    pub fn new(
        bytes: &BinaryEntry,
        addresses: MonotonicBlockPackedReaderRef,
        reverse_index: ReverseTermsIndexRef,
        data: Box<dyn IndexInput>,
    ) -> Result<CompressedBinaryDocValues> {
        let max_term_length = bytes.max_length;
        let num_reverse_index_values = reverse_index.term_addresses.size() as i64;
        let num_values = bytes.count;
        let num_index_values = addresses.size() as i64;

        let dv = CompressedBinaryDocValues {
            num_values,
            num_index_values,
            num_reverse_index_values,
            max_term_length,
            data,
            reverse_index,
            addresses,
        };
        Ok(dv)
    }

    pub fn lookup_term(&self, key: &[u8]) -> Result<i64> {
        let mut term_iterator = self.get_term_iterator()?;
        match term_iterator.seek_ceil(key)? {
            SeekStatus::Found => term_iterator.ord(),
            SeekStatus::NotFound => {
                let val = -term_iterator.ord()? - 1;
                Ok(val)
            }
            _ => Ok(-self.num_values - 1),
        }
    }

    pub fn get_term_iterator(&self) -> Result<CompressedBinaryTermIterator> {
        let data = IndexInput::clone(self.data.as_ref())?;
        CompressedBinaryTermIterator::new(
            data,
            self.max_term_length as usize,
            self.num_reverse_index_values,
            Arc::clone(&self.reverse_index),
            Arc::clone(&self.addresses),
            self.num_values,
            self.num_index_values,
        )
    }
}

impl LongBinaryDocValues for CompressedBinaryDocValues {
    fn get64(&self, id: i64) -> Result<Vec<u8>> {
        let mut term_iterator = self.get_term_iterator()?;
        term_iterator.seek_exact_ord(id)?;
        let term = term_iterator.term()?;
        Ok(term.to_vec())
    }
}

impl BinaryDocValues for CompressedBinaryDocValues {
    fn get(&self, doc_id: DocId) -> Result<Vec<u8>> {
        CompressedBinaryDocValues::get64(self, i64::from(doc_id))
    }
}

pub enum BoxedBinaryDocValuesEnum {
    General(Box<dyn LongBinaryDocValues>),
    Compressed(CompressedBinaryDocValues),
}
