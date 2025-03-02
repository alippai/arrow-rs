// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Contains declarations to bind to the [C Data Interface](https://arrow.apache.org/docs/format/CDataInterface.html).
//!
//! Generally, this module is divided in two main interfaces:
//! One interface maps C ABI to native Rust types, i.e. convert c-pointers, c_char, to native rust.
//! This is handled by [FFI_ArrowSchema] and [FFI_ArrowArray].
//!
//! The second interface maps native Rust types to the Rust-specific implementation of Arrow such as `format` to `Datatype`,
//! `Buffer`, etc. This is handled by `ArrowArray`.
//!
//! ```rust
//! # use std::sync::Arc;
//! # use arrow::array::{Int32Array, Array, ArrayData, make_array_from_raw};
//! # use arrow::error::{Result, ArrowError};
//! # use arrow::compute::kernels::arithmetic;
//! # use std::convert::TryFrom;
//! # fn main() -> Result<()> {
//! // create an array natively
//! let array = Int32Array::from(vec![Some(1), None, Some(3)]);
//!
//! // export it
//! let (array_ptr, schema_ptr) = array.to_raw()?;
//!
//! // consumed and used by something else...
//!
//! // import it
//! let array = unsafe { make_array_from_raw(array_ptr, schema_ptr)? };
//!
//! // perform some operation
//! let array = array.as_any().downcast_ref::<Int32Array>().ok_or(
//!     ArrowError::ParseError("Expects an int32".to_string()),
//! )?;
//! let array = arithmetic::add(&array, &array)?;
//!
//! // verify
//! assert_eq!(array, Int32Array::from(vec![Some(2), None, Some(6)]));
//!
//! // (drop/release)
//! Ok(())
//! }
//! ```

/*
# Design:

Main assumptions:
* A memory region is deallocated according it its own release mechanism.
* Rust shares memory regions between arrays.
* A memory region should be deallocated when no-one is using it.

The design of this module is as follows:

`ArrowArray` contains two `Arc`s, one per ABI-compatible `struct`, each containing data
according to the C Data Interface. These Arcs are used for ref counting of the structs
within Rust and lifetime management.

Each ABI-compatible `struct` knowns how to `drop` itself, calling `release`.

To import an array, unsafely create an `ArrowArray` from two pointers using [ArrowArray::try_from_raw].
To export an array, create an `ArrowArray` using [ArrowArray::try_new].
*/

use std::{
    ffi::CStr,
    ffi::CString,
    iter,
    mem::size_of,
    ptr::{self, NonNull},
    sync::Arc,
};

use crate::array::ArrayData;
use crate::buffer::Buffer;
use crate::datatypes::{DataType, Field, TimeUnit};
use crate::error::{ArrowError, Result};
use crate::util::bit_util;

#[allow(dead_code)]
struct SchemaPrivateData {
    field: Field,
    children_ptr: Box<[*mut FFI_ArrowSchema]>,
}

/// ABI-compatible struct for `ArrowSchema` from C Data Interface
/// See <https://arrow.apache.org/docs/format/CDataInterface.html#structure-definitions>
/// This was created by bindgen
#[repr(C)]
#[derive(Debug)]
pub struct FFI_ArrowSchema {
    format: *const ::std::os::raw::c_char,
    name: *const ::std::os::raw::c_char,
    metadata: *const ::std::os::raw::c_char,
    flags: i64,
    n_children: i64,
    children: *mut *mut FFI_ArrowSchema,
    dictionary: *mut FFI_ArrowSchema,
    release: ::std::option::Option<unsafe extern "C" fn(arg1: *mut FFI_ArrowSchema)>,
    private_data: *mut ::std::os::raw::c_void,
}

// callback used to drop [FFI_ArrowSchema] when it is exported.
unsafe extern "C" fn release_schema(schema: *mut FFI_ArrowSchema) {
    if schema.is_null() {
        return;
    }
    let schema = &mut *schema;

    // take ownership back to release it.
    CString::from_raw(schema.format as *mut std::os::raw::c_char);
    CString::from_raw(schema.name as *mut std::os::raw::c_char);
    let private = Box::from_raw(schema.private_data as *mut SchemaPrivateData);
    for child in private.children_ptr.iter() {
        let _ = Box::from_raw(*child);
    }

    schema.release = None;
}

impl FFI_ArrowSchema {
    /// create a new [`Ffi_ArrowSchema`]. This fails if the fields' [`DataType`] is not supported.
    fn try_new(field: Field) -> Result<FFI_ArrowSchema> {
        let format = to_format(field.data_type())?;
        let name = field.name().clone();

        // allocate (and hold) the children
        let children_vec = match field.data_type() {
            DataType::List(field) => {
                vec![Box::new(FFI_ArrowSchema::try_new(field.as_ref().clone())?)]
            }
            DataType::LargeList(field) => {
                vec![Box::new(FFI_ArrowSchema::try_new(field.as_ref().clone())?)]
            }
            DataType::Struct(fields) => fields
                .iter()
                .map(|field| Ok(Box::new(FFI_ArrowSchema::try_new(field.clone())?)))
                .collect::<Result<Vec<_>>>()?,
            _ => vec![],
        };
        // note: this cannot be done along with the above because the above is fallible and this op leaks.
        let children_ptr = children_vec
            .into_iter()
            .map(Box::into_raw)
            .collect::<Box<_>>();
        let n_children = children_ptr.len() as i64;

        let flags = field.is_nullable() as i64 * 2;

        let mut private = Box::new(SchemaPrivateData {
            field,
            children_ptr,
        });

        // <https://arrow.apache.org/docs/format/CDataInterface.html#c.ArrowSchema>
        Ok(FFI_ArrowSchema {
            format: CString::new(format).unwrap().into_raw(),
            name: CString::new(name).unwrap().into_raw(),
            metadata: std::ptr::null_mut(),
            flags,
            n_children,
            children: private.children_ptr.as_mut_ptr(),
            dictionary: std::ptr::null_mut(),
            release: Some(release_schema),
            private_data: Box::into_raw(private) as *mut ::std::os::raw::c_void,
        })
    }

    /// create an empty [FFI_ArrowSchema]
    fn empty() -> Self {
        Self {
            format: std::ptr::null_mut(),
            name: std::ptr::null_mut(),
            metadata: std::ptr::null_mut(),
            flags: 0,
            n_children: 0,
            children: ptr::null_mut(),
            dictionary: std::ptr::null_mut(),
            release: None,
            private_data: std::ptr::null_mut(),
        }
    }

    /// returns the format of this schema.
    pub fn format(&self) -> &str {
        assert!(!self.format.is_null());
        // safe because the lifetime of `self.format` equals `self`
        unsafe { CStr::from_ptr(self.format) }
            .to_str()
            .expect("The external API has a non-utf8 as format")
    }

    /// returns the name of this schema.
    pub fn name(&self) -> &str {
        assert!(!self.name.is_null());
        // safe because the lifetime of `self.name` equals `self`
        unsafe { CStr::from_ptr(self.name) }.to_str().unwrap()
    }

    pub fn child(&self, index: usize) -> &Self {
        assert!(index < self.n_children as usize);
        assert!(!self.name.is_null());
        unsafe { self.children.add(index).as_ref().unwrap().as_ref().unwrap() }
    }

    pub fn nullable(&self) -> bool {
        (self.flags / 2) & 1 == 1
    }
}

impl Drop for FFI_ArrowSchema {
    fn drop(&mut self) {
        match self.release {
            None => (),
            Some(release) => unsafe { release(self) },
        };
    }
}

/// See https://arrow.apache.org/docs/format/CDataInterface.html#data-type-description-format-strings
fn to_field(schema: &FFI_ArrowSchema) -> Result<Field> {
    let data_type = match schema.format() {
        "n" => DataType::Null,
        "b" => DataType::Boolean,
        "c" => DataType::Int8,
        "C" => DataType::UInt8,
        "s" => DataType::Int16,
        "S" => DataType::UInt16,
        "i" => DataType::Int32,
        "I" => DataType::UInt32,
        "l" => DataType::Int64,
        "L" => DataType::UInt64,
        "e" => DataType::Float16,
        "f" => DataType::Float32,
        "g" => DataType::Float64,
        "z" => DataType::Binary,
        "Z" => DataType::LargeBinary,
        "u" => DataType::Utf8,
        "U" => DataType::LargeUtf8,
        "tdD" => DataType::Date32,
        "tdm" => DataType::Date64,
        "tts" => DataType::Time32(TimeUnit::Second),
        "ttm" => DataType::Time32(TimeUnit::Millisecond),
        "ttu" => DataType::Time64(TimeUnit::Microsecond),
        "ttn" => DataType::Time64(TimeUnit::Nanosecond),
        "+l" => {
            let child = schema.child(0);
            DataType::List(Box::new(to_field(child)?))
        }
        "+L" => {
            let child = schema.child(0);
            DataType::LargeList(Box::new(to_field(child)?))
        }
        "+s" => {
            let children = (0..schema.n_children as usize)
                .map(|x| to_field(schema.child(x)))
                .collect::<Result<Vec<_>>>()?;
            DataType::Struct(children)
        }
        other => {
            return Err(ArrowError::CDataInterface(format!(
                "The datatype \"{:?}\" is still not supported in Rust implementation",
                other
            )))
        }
    };
    Ok(Field::new(schema.name(), data_type, schema.nullable()))
}

/// See https://arrow.apache.org/docs/format/CDataInterface.html#data-type-description-format-strings
fn to_format(data_type: &DataType) -> Result<String> {
    Ok(match data_type {
        DataType::Null => "n",
        DataType::Boolean => "b",
        DataType::Int8 => "c",
        DataType::UInt8 => "C",
        DataType::Int16 => "s",
        DataType::UInt16 => "S",
        DataType::Int32 => "i",
        DataType::UInt32 => "I",
        DataType::Int64 => "l",
        DataType::UInt64 => "L",
        DataType::Float16 => "e",
        DataType::Float32 => "f",
        DataType::Float64 => "g",
        DataType::Binary => "z",
        DataType::LargeBinary => "Z",
        DataType::Utf8 => "u",
        DataType::LargeUtf8 => "U",
        DataType::Date32 => "tdD",
        DataType::Date64 => "tdm",
        DataType::Time32(TimeUnit::Second) => "tts",
        DataType::Time32(TimeUnit::Millisecond) => "ttm",
        DataType::Time64(TimeUnit::Microsecond) => "ttu",
        DataType::Time64(TimeUnit::Nanosecond) => "ttn",
        DataType::List(_) => "+l",
        DataType::LargeList(_) => "+L",
        DataType::Struct(_) => "+s",
        z => {
            return Err(ArrowError::CDataInterface(format!(
                "The datatype \"{:?}\" is still not supported in Rust implementation",
                z
            )))
        }
    }
    .to_string())
}

// returns the number of bits that buffer `i` (in the C data interface) is expected to have.
// This is set by the Arrow specification
fn bit_width(data_type: &DataType, i: usize) -> Result<usize> {
    Ok(match (data_type, i) {
        // the null buffer is bit sized
        (_, 0) => 1,
        // primitive types first buffer's size is given by the native types
        (DataType::Boolean, 1) => 1,
        (DataType::UInt8, 1) => size_of::<u8>() * 8,
        (DataType::UInt16, 1) => size_of::<u16>() * 8,
        (DataType::UInt32, 1) => size_of::<u32>() * 8,
        (DataType::UInt64, 1) => size_of::<u64>() * 8,
        (DataType::Int8, 1) => size_of::<i8>() * 8,
        (DataType::Int16, 1) => size_of::<i16>() * 8,
        (DataType::Int32, 1) | (DataType::Date32, 1) | (DataType::Time32(_), 1) => size_of::<i32>() * 8,
        (DataType::Int64, 1) | (DataType::Date64, 1) | (DataType::Time64(_), 1) => size_of::<i64>() * 8,
        (DataType::Float32, 1) => size_of::<f32>() * 8,
        (DataType::Float64, 1) => size_of::<f64>() * 8,
        // primitive types have a single buffer
        (DataType::Boolean, _) |
        (DataType::UInt8, _) |
        (DataType::UInt16, _) |
        (DataType::UInt32, _) |
        (DataType::UInt64, _) |
        (DataType::Int8, _) |
        (DataType::Int16, _) |
        (DataType::Int32, _) | (DataType::Date32, _) | (DataType::Time32(_), _) |
        (DataType::Int64, _) | (DataType::Date64, _) | (DataType::Time64(_), _) |
        (DataType::Float32, _) |
        (DataType::Float64, _) => {
            return Err(ArrowError::CDataInterface(format!(
                "The datatype \"{:?}\" expects 2 buffers, but requested {}. Please verify that the C data interface is correctly implemented.",
                data_type, i
            )))
        }
        // Variable-sized binaries: have two buffers.
        // "small": first buffer is i32, second is in bytes
        (DataType::Utf8, 1) | (DataType::Binary, 1) | (DataType::List(_), 1) => size_of::<i32>() * 8,
        (DataType::Utf8, 2) | (DataType::Binary, 2) | (DataType::List(_), 2) => size_of::<u8>() * 8,
        (DataType::Utf8, _) | (DataType::Binary, _) | (DataType::List(_), _)=> {
            return Err(ArrowError::CDataInterface(format!(
                "The datatype \"{:?}\" expects 3 buffers, but requested {}. Please verify that the C data interface is correctly implemented.",
                data_type, i
            )))
        }
        // Variable-sized binaries: have two buffers.
        // LargeUtf8: first buffer is i64, second is in bytes
        (DataType::LargeUtf8, 1) | (DataType::LargeBinary, 1) | (DataType::LargeList(_), 1) => size_of::<i64>() * 8,
        (DataType::LargeUtf8, 2) | (DataType::LargeBinary, 2) | (DataType::LargeList(_), 2)=> size_of::<u8>() * 8,
        (DataType::LargeUtf8, _) | (DataType::LargeBinary, _) | (DataType::LargeList(_), _)=> {
            return Err(ArrowError::CDataInterface(format!(
                "The datatype \"{:?}\" expects 3 buffers, but requested {}. Please verify that the C data interface is correctly implemented.",
                data_type, i
            )))
        }
        _ => {
            return Err(ArrowError::CDataInterface(format!(
                "The datatype \"{:?}\" is still not supported in Rust implementation",
                data_type
            )))
        }
    })
}

/// ABI-compatible struct for ArrowArray from C Data Interface
/// See <https://arrow.apache.org/docs/format/CDataInterface.html#structure-definitions>
/// This was created by bindgen
#[repr(C)]
#[derive(Debug)]
pub struct FFI_ArrowArray {
    pub(crate) length: i64,
    pub(crate) null_count: i64,
    pub(crate) offset: i64,
    pub(crate) n_buffers: i64,
    pub(crate) n_children: i64,
    pub(crate) buffers: *mut *const ::std::os::raw::c_void,
    children: *mut *mut FFI_ArrowArray,
    dictionary: *mut FFI_ArrowArray,
    release: ::std::option::Option<unsafe extern "C" fn(arg1: *mut FFI_ArrowArray)>,
    // When exported, this MUST contain everything that is owned by this array.
    // for example, any buffer pointed to in `buffers` must be here, as well as the `buffers` pointer
    // itself.
    // In other words, everything in [FFI_ArrowArray] must be owned by `private_data` and can assume
    // that they do not outlive `private_data`.
    private_data: *mut ::std::os::raw::c_void,
}

impl Drop for FFI_ArrowArray {
    fn drop(&mut self) {
        match self.release {
            None => (),
            Some(release) => unsafe { release(self) },
        };
    }
}

// callback used to drop [FFI_ArrowArray] when it is exported
unsafe extern "C" fn release_array(array: *mut FFI_ArrowArray) {
    if array.is_null() {
        return;
    }
    let array = &mut *array;

    // take ownership of `private_data`, therefore dropping it`
    let private = Box::from_raw(array.private_data as *mut PrivateData);
    for child in private.children.iter() {
        let _ = Box::from_raw(*child);
    }

    array.release = None;
}

struct PrivateData {
    buffers: Vec<Option<Buffer>>,
    buffers_ptr: Box<[*const std::os::raw::c_void]>,
    children: Box<[*mut FFI_ArrowArray]>,
}

impl FFI_ArrowArray {
    /// creates a new `FFI_ArrowArray` from existing data.
    /// # Safety
    /// This method releases `buffers`. Consumers of this struct *must* call `release` before
    /// releasing this struct, or contents in `buffers` leak.
    fn new(data: &ArrayData) -> Self {
        // * insert the null buffer at the start
        // * make all others `Option<Buffer>`.
        let buffers = iter::once(data.null_buffer().cloned())
            .chain(data.buffers().iter().map(|b| Some(b.clone())))
            .collect::<Vec<_>>();
        let n_buffers = buffers.len() as i64;

        let buffers_ptr = buffers
            .iter()
            .map(|maybe_buffer| match maybe_buffer {
                // note that `raw_data` takes into account the buffer's offset
                Some(b) => b.as_ptr() as *const std::os::raw::c_void,
                None => std::ptr::null(),
            })
            .collect::<Box<[_]>>();

        let children = data
            .child_data()
            .iter()
            .map(|child| Box::into_raw(Box::new(FFI_ArrowArray::new(child))))
            .collect::<Box<_>>();
        let n_children = children.len() as i64;

        // create the private data owning everything.
        // any other data must be added here, e.g. via a struct, to track lifetime.
        let mut private_data = Box::new(PrivateData {
            buffers,
            buffers_ptr,
            children,
        });

        Self {
            length: data.len() as i64,
            null_count: data.null_count() as i64,
            offset: data.offset() as i64,
            n_buffers,
            n_children,
            buffers: private_data.buffers_ptr.as_mut_ptr(),
            children: private_data.children.as_mut_ptr(),
            dictionary: std::ptr::null_mut(),
            release: Some(release_array),
            private_data: Box::into_raw(private_data) as *mut ::std::os::raw::c_void,
        }
    }

    // create an empty `FFI_ArrowArray`, which can be used to import data into
    fn empty() -> Self {
        Self {
            length: 0,
            null_count: 0,
            offset: 0,
            n_buffers: 0,
            n_children: 0,
            buffers: std::ptr::null_mut(),
            children: std::ptr::null_mut(),
            dictionary: std::ptr::null_mut(),
            release: None,
            private_data: std::ptr::null_mut(),
        }
    }

    /// the length of the array
    pub fn len(&self) -> usize {
        self.length as usize
    }

    /// whether the array is empty
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// the offset of the array
    pub fn offset(&self) -> usize {
        self.offset as usize
    }

    /// the null count of the array
    pub fn null_count(&self) -> usize {
        self.null_count as usize
    }
}

/// returns a new buffer corresponding to the index `i` of the FFI array. It may not exist (null pointer).
/// `bits` is the number of bits that the native type of this buffer has.
/// The size of the buffer will be `ceil(self.length * bits, 8)`.
/// # Panic
/// This function panics if `i` is larger or equal to `n_buffers`.
/// # Safety
/// This function assumes that `ceil(self.length * bits, 8)` is the size of the buffer
unsafe fn create_buffer(
    owner: Arc<FFI_ArrowArray>,
    array: &FFI_ArrowArray,
    index: usize,
    len: usize,
) -> Option<Buffer> {
    if array.buffers.is_null() {
        return None;
    }
    let buffers = array.buffers as *mut *const u8;

    assert!(index < array.n_buffers as usize);
    let ptr = *buffers.add(index);

    NonNull::new(ptr as *mut u8).map(|ptr| Buffer::from_unowned(ptr, len, owner))
}

fn create_child(
    owner: Arc<FFI_ArrowArray>,
    array: &FFI_ArrowArray,
    schema: &FFI_ArrowSchema,
    index: usize,
) -> ArrowArrayChild<'static> {
    assert!(index < array.n_children as usize);
    assert!(!array.children.is_null());
    assert!(!array.children.is_null());
    unsafe {
        let arr_ptr = *array.children.add(index);
        let schema_ptr = *schema.children.add(index);
        assert!(!arr_ptr.is_null());
        assert!(!schema_ptr.is_null());
        let arr_ptr = &*arr_ptr;
        let schema_ptr = &*schema_ptr;
        ArrowArrayChild::from_raw(arr_ptr, schema_ptr, owner)
    }
}

pub trait ArrowArrayRef {
    fn to_data(&self) -> Result<ArrayData> {
        let data_type = self.data_type()?;
        let len = self.array().len();
        let offset = self.array().offset();
        let null_count = self.array().null_count();
        let buffers = self.buffers()?;
        let null_bit_buffer = self.null_bit_buffer();

        let child_data = (0..self.array().n_children as usize)
            .map(|i| {
                let child = self.child(i);
                child.to_data()
            })
            .map(|d| d.unwrap())
            .collect();

        Ok(ArrayData::new(
            data_type,
            len,
            Some(null_count),
            null_bit_buffer,
            offset,
            buffers,
            child_data,
        ))
    }

    /// returns all buffers, as organized by Rust (i.e. null buffer is skipped)
    fn buffers(&self) -> Result<Vec<Buffer>> {
        (0..self.array().n_buffers - 1)
            .map(|index| {
                // + 1: skip null buffer
                let index = (index + 1) as usize;

                let len = self.buffer_len(index)?;

                unsafe { create_buffer(self.owner().clone(), self.array(), index, len) }
                    .ok_or_else(|| {
                        ArrowError::CDataInterface(format!(
                            "The external buffer at position {} is null.",
                            index - 1
                        ))
                    })
            })
            .collect()
    }

    /// Returns the length, in bytes, of the buffer `i` (indexed according to the C data interface)
    // Rust implementation uses fixed-sized buffers, which require knowledge of their `len`.
    // for variable-sized buffers, such as the second buffer of a stringArray, we need
    // to fetch offset buffer's len to build the second buffer.
    fn buffer_len(&self, i: usize) -> Result<usize> {
        // Inner type is not important for buffer length.
        let data_type = &self.data_type()?;

        Ok(match (data_type, i) {
            (DataType::Utf8, 1)
            | (DataType::LargeUtf8, 1)
            | (DataType::Binary, 1)
            | (DataType::LargeBinary, 1)
            | (DataType::List(_), 1)
            | (DataType::LargeList(_), 1) => {
                // the len of the offset buffer (buffer 1) equals length + 1
                let bits = bit_width(data_type, i)?;
                debug_assert_eq!(bits % 8, 0);
                (self.array().length as usize + 1) * (bits / 8)
            }
            (DataType::Utf8, 2) | (DataType::Binary, 2) | (DataType::List(_), 2) => {
                // the len of the data buffer (buffer 2) equals the last value of the offset buffer (buffer 1)
                let len = self.buffer_len(1)?;
                // first buffer is the null buffer => add(1)
                // we assume that pointer is aligned for `i32`, as Utf8 uses `i32` offsets.
                #[allow(clippy::cast_ptr_alignment)]
                let offset_buffer = unsafe {
                    *(self.array().buffers as *mut *const u8).add(1) as *const i32
                };
                // get last offset
                (unsafe { *offset_buffer.add(len / size_of::<i32>() - 1) }) as usize
            }
            (DataType::LargeUtf8, 2)
            | (DataType::LargeBinary, 2)
            | (DataType::LargeList(_), 2) => {
                // the len of the data buffer (buffer 2) equals the last value of the offset buffer (buffer 1)
                let len = self.buffer_len(1)?;
                // first buffer is the null buffer => add(1)
                // we assume that pointer is aligned for `i64`, as Large uses `i64` offsets.
                #[allow(clippy::cast_ptr_alignment)]
                let offset_buffer = unsafe {
                    *(self.array().buffers as *mut *const u8).add(1) as *const i64
                };
                // get last offset
                (unsafe { *offset_buffer.add(len / size_of::<i64>() - 1) }) as usize
            }
            // buffer len of primitive types
            _ => {
                let bits = bit_width(data_type, i)?;
                bit_util::ceil(self.array().length as usize * bits, 8)
            }
        })
    }

    /// returns the null bit buffer.
    /// Rust implementation uses a buffer that is not part of the array of buffers.
    /// The C Data interface's null buffer is part of the array of buffers.
    fn null_bit_buffer(&self) -> Option<Buffer> {
        // similar to `self.buffer_len(0)`, but without `Result`.
        let buffer_len = bit_util::ceil(self.array().length as usize, 8);

        unsafe { create_buffer(self.owner().clone(), self.array(), 0, buffer_len) }
    }

    fn child(&self, index: usize) -> ArrowArrayChild {
        create_child(self.owner().clone(), self.array(), self.schema(), index)
    }

    fn owner(&self) -> &Arc<FFI_ArrowArray>;
    fn array(&self) -> &FFI_ArrowArray;
    fn schema(&self) -> &FFI_ArrowSchema;
    fn data_type(&self) -> Result<DataType>;
}

/// Struct used to move an Array from and to the C Data Interface.
/// Its main responsibility is to expose functionality that requires
/// both [FFI_ArrowArray] and [FFI_ArrowSchema].
///
/// This struct has two main paths:
///
/// ## Import from the C Data Interface
/// * [ArrowArray::empty] to allocate memory to be filled by an external call
/// * [ArrowArray::try_from_raw] to consume two non-null allocated pointers
/// ## Export to the C Data Interface
/// * [ArrowArray::try_new] to create a new [ArrowArray] from Rust-specific information
/// * [ArrowArray::into_raw] to expose two pointers for [FFI_ArrowArray] and [FFI_ArrowSchema].
///
/// # Safety
/// Whoever creates this struct is responsible for releasing their resources. Specifically,
/// consumers *must* call [ArrowArray::into_raw] and take ownership of the individual pointers,
/// calling [FFI_ArrowArray::release] and [FFI_ArrowSchema::release] accordingly.
///
/// Furthermore, this struct assumes that the incoming data agrees with the C data interface.
#[derive(Debug)]
pub struct ArrowArray {
    array: Arc<FFI_ArrowArray>,
    schema: Arc<FFI_ArrowSchema>,
}

#[derive(Debug)]
pub struct ArrowArrayChild<'a> {
    array: &'a FFI_ArrowArray,
    schema: &'a FFI_ArrowSchema,
    owner: Arc<FFI_ArrowArray>,
}

impl ArrowArrayRef for ArrowArray {
    /// the data_type as declared in the schema
    fn data_type(&self) -> Result<DataType> {
        to_field(&self.schema).map(|x| x.data_type().clone())
    }

    fn array(&self) -> &FFI_ArrowArray {
        self.array.as_ref()
    }

    fn schema(&self) -> &FFI_ArrowSchema {
        self.schema.as_ref()
    }

    fn owner(&self) -> &Arc<FFI_ArrowArray> {
        &self.array
    }
}

impl<'a> ArrowArrayRef for ArrowArrayChild<'a> {
    /// the data_type as declared in the schema
    fn data_type(&self) -> Result<DataType> {
        to_field(self.schema).map(|x| x.data_type().clone())
    }

    fn array(&self) -> &FFI_ArrowArray {
        self.array
    }

    fn schema(&self) -> &FFI_ArrowSchema {
        self.schema
    }

    fn owner(&self) -> &Arc<FFI_ArrowArray> {
        &self.owner
    }
}

impl ArrowArray {
    /// creates a new `ArrowArray`. This is used to export to the C Data Interface.
    /// # Safety
    /// See safety of [ArrowArray]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn try_new(data: ArrayData) -> Result<Self> {
        let field = Field::new("", data.data_type().clone(), data.null_count() != 0);
        let array = Arc::new(FFI_ArrowArray::new(&data));
        let schema = Arc::new(FFI_ArrowSchema::try_new(field)?);

        Ok(ArrowArray { array, schema })
    }

    /// creates a new [ArrowArray] from two pointers. Used to import from the C Data Interface.
    /// # Safety
    /// See safety of [ArrowArray]
    /// # Error
    /// Errors if any of the pointers is null
    pub unsafe fn try_from_raw(
        array: *const FFI_ArrowArray,
        schema: *const FFI_ArrowSchema,
    ) -> Result<Self> {
        if array.is_null() || schema.is_null() {
            return Err(ArrowError::MemoryError(
                "At least one of the pointers passed to `try_from_raw` is null"
                    .to_string(),
            ));
        };
        Ok(Self {
            array: Arc::from_raw(array as *mut FFI_ArrowArray),
            schema: Arc::from_raw(schema as *mut FFI_ArrowSchema),
        })
    }

    /// creates a new empty [ArrowArray]. Used to import from the C Data Interface.
    /// # Safety
    /// See safety of [ArrowArray]
    pub unsafe fn empty() -> Self {
        let schema = Arc::new(FFI_ArrowSchema::empty());
        let array = Arc::new(FFI_ArrowArray::empty());
        ArrowArray { array, schema }
    }

    /// exports [ArrowArray] to the C Data Interface
    pub fn into_raw(this: ArrowArray) -> (*const FFI_ArrowArray, *const FFI_ArrowSchema) {
        (Arc::into_raw(this.array), Arc::into_raw(this.schema))
    }
}

impl<'a> ArrowArrayChild<'a> {
    fn from_raw(
        array: &'a FFI_ArrowArray,
        schema: &'a FFI_ArrowSchema,
        owner: Arc<FFI_ArrowArray>,
    ) -> Self {
        Self {
            array,
            schema,
            owner,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array::{
        make_array, Array, ArrayData, BinaryOffsetSizeTrait, BooleanArray,
        GenericBinaryArray, GenericListArray, GenericStringArray, Int32Array,
        OffsetSizeTrait, StringOffsetSizeTrait, Time32MillisecondArray,
    };
    use crate::compute::kernels;
    use crate::datatypes::Field;
    use std::convert::TryFrom;

    #[test]
    fn test_round_trip() -> Result<()> {
        // create an array natively
        let array = Int32Array::from(vec![1, 2, 3]);

        // export it
        let array = ArrowArray::try_from(array.data().clone())?;

        // (simulate consumer) import it
        let data = ArrayData::try_from(array)?;
        let array = make_array(data);

        // perform some operation
        let array = array.as_any().downcast_ref::<Int32Array>().unwrap();
        let array = kernels::arithmetic::add(&array, &array).unwrap();

        // verify
        assert_eq!(array, Int32Array::from(vec![2, 4, 6]));

        // (drop/release)
        Ok(())
    }
    // case with nulls is tested in the docs, through the example on this module.

    fn test_generic_string<Offset: StringOffsetSizeTrait>() -> Result<()> {
        // create an array natively
        let array =
            GenericStringArray::<Offset>::from(vec![Some("a"), None, Some("aaa")]);

        // export it
        let array = ArrowArray::try_from(array.data().clone())?;

        // (simulate consumer) import it
        let data = ArrayData::try_from(array)?;
        let array = make_array(data);

        // perform some operation
        let array = kernels::concat::concat(&[array.as_ref(), array.as_ref()]).unwrap();
        let array = array
            .as_any()
            .downcast_ref::<GenericStringArray<Offset>>()
            .unwrap();

        // verify
        let expected = GenericStringArray::<Offset>::from(vec![
            Some("a"),
            None,
            Some("aaa"),
            Some("a"),
            None,
            Some("aaa"),
        ]);
        assert_eq!(array, &expected);

        // (drop/release)
        Ok(())
    }

    #[test]
    fn test_string() -> Result<()> {
        test_generic_string::<i32>()
    }

    #[test]
    fn test_large_string() -> Result<()> {
        test_generic_string::<i64>()
    }

    fn test_generic_list<Offset: OffsetSizeTrait>() -> Result<()> {
        // Construct a value array
        let value_data = ArrayData::builder(DataType::Int32)
            .len(8)
            .add_buffer(Buffer::from_slice_ref(&[0, 1, 2, 3, 4, 5, 6, 7]))
            .build();

        // Construct a buffer for value offsets, for the nested array:
        //  [[0, 1, 2], [3, 4, 5], [6, 7]]
        let value_offsets = [0_usize, 3, 6, 8]
            .iter()
            .map(|i| Offset::from_usize(*i).unwrap())
            .collect::<Buffer>();

        // Construct a list array from the above two
        let list_data_type = match std::mem::size_of::<Offset>() {
            4 => DataType::List(Box::new(Field::new("item", DataType::Int32, false))),
            _ => {
                DataType::LargeList(Box::new(Field::new("item", DataType::Int32, false)))
            }
        };

        let list_data = ArrayData::builder(list_data_type)
            .len(3)
            .add_buffer(value_offsets)
            .add_child_data(value_data)
            .build();

        // create an array natively
        let array = GenericListArray::<Offset>::from(list_data.clone());

        // export it
        let array = ArrowArray::try_from(array.data().clone())?;

        // (simulate consumer) import it
        let data = ArrayData::try_from(array)?;
        let array = make_array(data);

        // downcast
        let array = array
            .as_any()
            .downcast_ref::<GenericListArray<Offset>>()
            .unwrap();

        dbg!(&array);

        // verify
        let expected = GenericListArray::<Offset>::from(list_data);
        assert_eq!(&array.value(0), &expected.value(0));
        assert_eq!(&array.value(1), &expected.value(1));
        assert_eq!(&array.value(2), &expected.value(2));

        // (drop/release)
        Ok(())
    }

    #[test]
    fn test_list() -> Result<()> {
        test_generic_list::<i32>()
    }

    #[test]
    fn test_large_list() -> Result<()> {
        test_generic_list::<i64>()
    }

    fn test_generic_binary<Offset: BinaryOffsetSizeTrait>() -> Result<()> {
        // create an array natively
        let array: Vec<Option<&[u8]>> = vec![Some(b"a"), None, Some(b"aaa")];
        let array = GenericBinaryArray::<Offset>::from(array);

        // export it
        let array = ArrowArray::try_from(array.data().clone())?;

        // (simulate consumer) import it
        let data = ArrayData::try_from(array)?;
        let array = make_array(data);

        // perform some operation
        let array = kernels::concat::concat(&[array.as_ref(), array.as_ref()]).unwrap();
        let array = array
            .as_any()
            .downcast_ref::<GenericBinaryArray<Offset>>()
            .unwrap();

        // verify
        let expected: Vec<Option<&[u8]>> = vec![
            Some(b"a"),
            None,
            Some(b"aaa"),
            Some(b"a"),
            None,
            Some(b"aaa"),
        ];
        let expected = GenericBinaryArray::<Offset>::from(expected);
        assert_eq!(array, &expected);

        // (drop/release)
        Ok(())
    }

    #[test]
    fn test_binary() -> Result<()> {
        test_generic_binary::<i32>()
    }

    #[test]
    fn test_large_binary() -> Result<()> {
        test_generic_binary::<i64>()
    }

    #[test]
    fn test_bool() -> Result<()> {
        // create an array natively
        let array = BooleanArray::from(vec![None, Some(true), Some(false)]);

        // export it
        let array = ArrowArray::try_from(array.data().clone())?;

        // (simulate consumer) import it
        let data = ArrayData::try_from(array)?;
        let array = make_array(data);

        // perform some operation
        let array = array.as_any().downcast_ref::<BooleanArray>().unwrap();
        let array = kernels::boolean::not(&array)?;

        // verify
        assert_eq!(
            array,
            BooleanArray::from(vec![None, Some(false), Some(true)])
        );

        // (drop/release)
        Ok(())
    }

    #[test]
    fn test_time32() -> Result<()> {
        // create an array natively
        let array = Time32MillisecondArray::from(vec![None, Some(1), Some(2)]);

        // export it
        let array = ArrowArray::try_from(array.data().clone())?;

        // (simulate consumer) import it
        let data = ArrayData::try_from(array)?;
        let array = make_array(data);

        // perform some operation
        let array = kernels::concat::concat(&[array.as_ref(), array.as_ref()]).unwrap();
        let array = array
            .as_any()
            .downcast_ref::<Time32MillisecondArray>()
            .unwrap();

        // verify
        assert_eq!(
            array,
            &Time32MillisecondArray::from(vec![
                None,
                Some(1),
                Some(2),
                None,
                Some(1),
                Some(2)
            ])
        );

        // (drop/release)
        Ok(())
    }
}
