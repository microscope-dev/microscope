use std::sync::Arc;

use super::layout::{Missing, constant, offset, symbol};
use super::{read_unsigned, word};
use crate::runtime_model::{DynamicValue, HeldPlace, RuntimeImage, RuntimeStop, StoredValue};
use crate::{ImageAddress, VirtualAddress};

const MAX_MODULES: usize = 256;

const EMPTY: &str = "runtime.eface";
const METHODS: &str = "runtime.iface";

#[derive(Debug, Clone)]
pub struct TypeTables {
    first_module: ImageAddress,
    types: u64,
    end_types: u64,
    next_module: u64,
}

impl TypeTables {
    pub fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        const MODULE: &str = "runtime.moduledata";
        Ok(Self {
            first_module: symbol(image, "runtime.firstmoduledata")?,
            types: offset(image, MODULE, &["types"], 8)?,
            end_types: offset(image, MODULE, &["etypes"], 8)?,
            next_module: offset(image, MODULE, &["next"], 8)?,
        })
    }

    pub fn base(&self, stop: &dyn RuntimeStop, ty: u64) -> Result<u64, Arc<str>> {
        let read = |address: u64| {
            word(stop, VirtualAddress::new(address))
                .ok_or_else(|| Arc::<str>::from("a module's type table is unreadable"))
        };

        let mut module = stop.load_bias().wrapping_load(self.first_module).get();
        for _ in 0..MAX_MODULES {
            if module == 0 {
                break;
            }

            let types = read(module.wrapping_add(self.types))?;
            let end = read(module.wrapping_add(self.end_types))?;
            if (types..end).contains(&ty) {
                return Ok(types);
            }

            module = read(module.wrapping_add(self.next_module))?;
        }

        Err(format!("the type at {ty:#x} is in no module's types").into())
    }
}

#[derive(Debug, Clone)]
pub struct Interfaces {
    tables: TypeTables,

    empty_type: u64,
    empty_data: u64,
    methods_table: u64,
    methods_data: u64,

    table_type: u64,

    direct: u64,
    direct_bit: u64,
}

impl Interfaces {
    pub fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        const TYPE: &str = "internal/abi.Type";
        let (direct, direct_bit) = match constant(image, "internal/abi.TFlagDirectIface") {
            Ok(bit) => (offset(image, TYPE, &["TFlag"], 1)?, bit),
            Err(_) => (
                offset(image, TYPE, &["Kind_"], 1)?,
                constant(image, "internal/abi.KindDirectIface")?,
            ),
        };

        let table_type = offset(image, "internal/abi.ITab", &["Type"], 8)
            .or_else(|_| offset(image, "runtime.itab", &["_type"], 8))?;

        Ok(Self {
            tables: TypeTables::bind(image)?,
            empty_type: offset(image, EMPTY, &["_type"], 8)?,
            empty_data: offset(image, EMPTY, &["data"], 8)?,
            methods_table: offset(image, METHODS, &["tab"], 8)?,
            methods_data: offset(image, METHODS, &["data"], 8)?,
            table_type,
            direct,
            direct_bit,
        })
    }

    pub fn value(
        &self,
        stop: &dyn RuntimeStop,
        representation: &str,
        value: StoredValue<'_>,
    ) -> Option<Result<DynamicValue, Arc<str>>> {
        let read = |at: u64, what: &str| {
            word(stop, VirtualAddress::new(at))
                .ok_or_else(|| Arc::<str>::from(format!("{what} is unreadable")))
        };

        let field = |offset: u64, what: &str| match value {
            StoredValue::Memory(address) => read(address.get().wrapping_add(offset), what),
            StoredValue::Bytes(bytes) => usize::try_from(offset)
                .ok()
                .and_then(|offset| bytes.get(offset..)?.first_chunk::<8>().copied())
                .map(u64::from_le_bytes)
                .ok_or_else(|| format!("{what} is past the interface's bytes").into()),
        };

        let (descriptor, data) = match representation {
            EMPTY => (
                field(self.empty_type, "an interface's type"),
                self.empty_data,
            ),
            METHODS => (
                field(self.methods_table, "an interface's method table").and_then(|table| {
                    if table == 0 {
                        Ok(0)
                    } else {
                        read(table.wrapping_add(self.table_type), "a method table's type")
                    }
                }),
                self.methods_data,
            ),
            _ => return None,
        };

        Some((|| {
            let descriptor = descriptor?;
            if descriptor == 0 {
                return Ok(DynamicValue::Nil);
            }

            let base = self.tables.base(stop, descriptor)?;
            let flags = read_unsigned(
                stop,
                VirtualAddress::new(descriptor.wrapping_add(self.direct)),
                1,
            )
            .ok_or("a type's flags are unreadable")?;

            let place = match (flags & self.direct_bit != 0, value) {
                (true, StoredValue::Memory(address)) => {
                    HeldPlace::Memory(VirtualAddress::new(address.get().wrapping_add(data)))
                }
                (true, StoredValue::Bytes(_)) => HeldPlace::Within(data),
                (false, _) => {
                    HeldPlace::Memory(VirtualAddress::new(field(data, "an interface's data")?))
                }
            };

            Ok(DynamicValue::Held {
                descriptor: VirtualAddress::new(descriptor),
                offset: descriptor - base,
                place,
            })
        })())
    }
}
