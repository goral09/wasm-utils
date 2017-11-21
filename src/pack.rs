use parity_wasm::elements::{self, Section, Opcode, DataSegment, InitExpr, Internal};
use parity_wasm::builder;
use super::{CREATE_SYMBOL, CALL_SYMBOL};

/// Pack error.
///
/// Pack has number of assumptions of passed module structure.
/// When they are violated, pack_instance returns one of these.
#[derive(Debug)]
pub enum Error {
    MalformedModule,
    NoTypeSection,
    NoExportSection,
    NoCodeSection,
    InvalidCreateSignature,
    NoCreateSymbol,
    InvalidCreateMember,
}

/// If module has an exported "_create" function we want to pack it into "constructor".
/// `raw_module` is the actual contract code
/// `ctor_module` is the constructor which should return `raw_module`
pub fn pack_instance(raw_module: Vec<u8>, mut ctor_module: elements::Module) -> Result<elements::Module, Error> {

    // Total number of constructor module import functions
    let ctor_import_functions = ctor_module.import_section().map(|x| x.functions()).unwrap_or(0);

    // We need to find an internal ID of function witch is exported as "_create"
    // in order to find it in the Code section of the module
    let create_func_id = {
        let found_entry = ctor_module.export_section().ok_or(Error::NoExportSection)?.entries().iter()
            .find(|entry| CREATE_SYMBOL == entry.field()).ok_or(Error::NoCreateSymbol)?;

        let function_index: usize = match found_entry.internal() {
            &Internal::Function(index) => index as usize,
            _ => { return Err(Error::InvalidCreateMember) },
        };

        // Constructor should be of signature `func(i32)` (void), fail otherwise
        let type_id = ctor_module.function_section().ok_or(Error::NoCodeSection)?
            .entries().get(function_index).ok_or(Error::MalformedModule)?
            .type_ref();

        match ctor_module.type_section().ok_or(Error::NoTypeSection)?
            .types().get(type_id as usize).ok_or(Error::MalformedModule)?
        {
            &elements::Type::Function(ref f) => {
                if f.params().len() != 1 || f.params()[0] != elements::ValueType::I32 {
                    return Err(Error::InvalidCreateSignature);
                }
                if f.return_type().is_some() {
                    return Err(Error::InvalidCreateSignature);
                }
            }
        };

        // Calculates a function index within module's function section
        function_index - ctor_import_functions
    };

    // If new function is put in ctor module, it will have this callable index
    let last_function_index = ctor_module.function_section().map(|x| x.entries().len()).unwrap_or(0)
        + ctor_import_functions;

    // Code data address is an address where we put the contract's code (raw_module)
    let mut code_data_address = 0i32;

    for section in ctor_module.sections_mut() {
        match section {
            // TODO: add data section is there no one
            &mut Section::Data(ref mut data_section) => {
                let (index, offset) = if let Some(ref entry) = data_section.entries().iter().last() {
                    if let Opcode::I32Const(offst) = entry.offset().code()[0] {
                        let len = entry.value().len() as i32;
                        let offst = offst as i32;
                        (entry.index(), offst + (len + 4) - len % 4)
                    } else {
                        (0, 0)
                    }
                } else {
                    (0, 0)
                };
                let code_data = DataSegment::new(
                    index,
                    InitExpr::new(vec![Opcode::I32Const(offset), Opcode::End]),
                    raw_module.clone()
                );
                data_section.entries_mut().push(code_data);
                code_data_address = offset;
            },
            _ => {;}
        }
    }

    let mut new_module = builder::from_module(ctor_module)
        .function()
        .signature().param().i32().build()
        .body().with_opcodes(elements::Opcodes::new(
            vec![
                Opcode::GetLocal(0),
                Opcode::Call(create_func_id as u32),
                Opcode::GetLocal(0),
                Opcode::I32Const(code_data_address),
                Opcode::I32Store(0, 8),
                Opcode::GetLocal(0),
                Opcode::I32Const(raw_module.len() as i32),
                Opcode::I32Store(0, 12),
                Opcode::End,
            ])).build()
            .build()
        .build();

    for section in new_module.sections_mut() {
        match section {
            &mut Section::Export(ref mut export_section) => {
                for entry in export_section.entries_mut().iter_mut() {
                    if CREATE_SYMBOL == entry.field() {
                        // change _create export name into default _call
                        *entry.field_mut() = CALL_SYMBOL.to_owned();
                        *entry.internal_mut() = elements::Internal::Function(last_function_index as u32);
                    }
                }
            },
            _ => { },
        }
    };

    Ok(new_module)
}

#[cfg(test)]
mod test {
    extern crate parity_wasm;
    extern crate byteorder;

    use parity_wasm::builder;
    use parity_wasm::interpreter;
    use parity_wasm::interpreter::RuntimeValue;
    use parity_wasm::ModuleInstanceInterface;
    use super::*;
    use super::super::optimize;
    use byteorder::{ByteOrder, LittleEndian};

    #[test]
    fn call_returns_code() {
        let mut module = builder::module()
            .import()
                .module("env")
                .field("memory")
                .external()
                .memory(1 as u32, Some(1 as u32))
            .build()
            .data()
                .offset(elements::Opcode::I32Const(16))
                .value(vec![0u8])
            .build()
            .function()
                .signature().build()
                .body()
                    .with_opcodes(elements::Opcodes::new(
                        vec![
                            elements::Opcode::End
                        ]
                    ))
                    .build()
            .build()
            .function()
                .signature().param().i32().build()
                .body()
                    .with_opcodes(elements::Opcodes::new(
                        vec![
                            elements::Opcode::End
                        ]
                    ))
                    .build()
            .build()
            .export()
                .field(CALL_SYMBOL)
                .internal().func(0)
            .build()
            .export()
                .field(CREATE_SYMBOL)
                .internal().func(1)
            .build()
        .build();

        let mut ctor_module = module.clone();
        optimize(&mut module, vec![CALL_SYMBOL]).expect("Optimizer to finish without errors");
        optimize(&mut ctor_module, vec![CREATE_SYMBOL]).expect("Optimizer to finish without errors");

        let raw_module = parity_wasm::serialize(module).unwrap();
        let ctor_module = pack_instance(raw_module.clone(), ctor_module).expect("Packing failed");

        let program = parity_wasm::DefaultProgramInstance::new().expect("Program instance failed to load");
        let env_instance = program.module("env").expect("Wasm program to contain env module");
        let env_memory = env_instance.memory(interpreter::ItemIndex::Internal(0)).expect("Linear memory to exist in wasm runtime");

        let execution_params = interpreter::ExecutionParams::default();
        let constructor_module = program.add_module("contract", ctor_module, None).expect("Failed to initialize module");

        let _ = constructor_module.execute_export(CALL_SYMBOL, execution_params.add_argument(RuntimeValue::I32(1024)));

        let pointer = LittleEndian::read_u32(&env_memory.get(1024 + 8, 4).unwrap());
        let len = LittleEndian::read_u32(&env_memory.get(1024 + 12, 4).unwrap());

        let contract_code = env_memory.get(pointer, len as usize).expect("Failed to get code");

        assert_eq!(raw_module, contract_code);

        let contract_module: elements::Module = parity_wasm::deserialize_buffer(contract_code).expect("Constructed contract module is not valid");

        let program = parity_wasm::DefaultProgramInstance::new().expect("Program2 instance failed to load");
        let contract_module_instance = program.add_module("contract", contract_module, None).expect("Failed to initialize constructed contract module");
        let execution_params = interpreter::ExecutionParams::default();

        contract_module_instance.execute_export(CALL_SYMBOL, execution_params).expect("Constructed contract failed to execute");
    }
}
