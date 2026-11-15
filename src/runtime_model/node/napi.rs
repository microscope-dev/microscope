use super::objects::{Problem, Reader};
use crate::image::v8::{Constant, Field, V8Version};

pub(super) const INVOKE: &str = "_ZN6v8impl12_GLOBAL__N_123FunctionCallbackWrapper6InvokeERKN2v820FunctionCallbackInfoINS2_5ValueEEE";

const IMPLICIT_SLOT: u64 = 4;

const TARGET_WORD: u64 = 8;

const BUNDLE_CALLBACK: u64 = 2;

pub(super) fn callback(reader: &Reader<'_>, version: V8Version, info: u64) -> Result<u64, Problem> {
    let data = if version.major == 12 {
        let implicit = reader.word(info)?;
        reader.word(implicit.wrapping_add(8 * IMPLICIT_SLOT))?
    } else {
        let target = if version.major == 13 {
            let implicit = reader.word(info)?;
            reader.word(implicit.wrapping_add(8 * IMPLICIT_SLOT))?
        } else {
            reader.word(info.wrapping_add(8 * TARGET_WORD))?
        };
        let template = reader
            .object(target)?
            .ok_or("the call's target is no object")?;
        if !reader.is(template, Constant::FunctionTemplateInfoType)? {
            return Err("the call's target is no function template".into());
        }
        reader.field(template, Field::FunctionTemplateInfoCallbackData)?
    };
    let data = reader
        .object(data)?
        .ok_or("the function's data is no object")?;
    if !reader.is(data, Constant::JSExternalObjectType)? {
        return Err(
            "the function's data is no external object, so it is no Node-API function".into(),
        );
    }
    let bundle = reader.field(data, Field::JSExternalObjectValue)?;
    if bundle == 0 {
        return Err("the function's external object holds no bundle".into());
    }
    let callback = reader.word(bundle.wrapping_add(8 * BUNDLE_CALLBACK))?;
    if callback == 0 {
        return Err("the function's bundle holds no callback".into());
    }
    Ok(callback)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::objects::Reader;
    use crate::image::v8::{V8Layout, V8Version};
    use crate::runtime_model::RuntimeStop;
    use crate::{LoadBias, ThreadId, VirtualAddress};

    #[derive(Default)]
    struct Memory(HashMap<u64, u8>);

    impl Memory {
        fn word(&mut self, address: u64, word: u64) {
            for (at, byte) in (address..).zip(word.to_le_bytes()) {
                self.0.insert(at, byte);
            }
        }
    }

    impl RuntimeStop for Memory {
        fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
            for (at, byte) in (address.get()..).zip(bytes.iter_mut()) {
                let Some(read) = self.0.get(&at) else {
                    return false;
                };
                *byte = *read;
            }
            true
        }

        fn thread_pointer(&self, _thread: ThreadId) -> Option<u64> {
            None
        }

        fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
            None
        }

        fn load_bias(&self) -> LoadBias {
            LoadBias::default()
        }

        fn threads(&self) -> Vec<ThreadId> {
            Vec::new()
        }
    }

    fn reader<'a>(memory: &'a Memory, layout: &'a V8Layout, major: u32) -> Reader<'a> {
        Reader {
            stop: memory,
            layout,
            version: V8Version {
                major,
                ..V8Version::default()
            },
            charge: None,
            blocks: None,
        }
    }

    #[test]
    fn an_info_near_the_top_of_the_address_space_is_unreadable() {
        let layout = crate::debug_info::v8::generated_layout(14, 6).expect("a layout");
        let memory = Memory::default();
        let top = reader(&memory, &layout, 14);
        assert!(super::callback(&top, top.version, u64::MAX - 8).is_err());

        let mut memory = Memory::default();
        memory.word(0x1000, u64::MAX - 8);
        let implicit = reader(&memory, &layout, 12);
        assert!(super::callback(&implicit, implicit.version, 0x1000).is_err());
    }
}
