use clap::Parser;
use std::collections::HashMap;
use std::{io::Write, path::PathBuf};

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum DisassemblyFormat {
    Guest,
    GuestAndNative,
    Native,
    DiffFriendly,
}

#[derive(Parser, Debug)]
#[clap(version)]
enum Args {
    /// Links a given ELF file into a `.polkavm` program blob.
    Link {
        /// The output file.
        #[clap(short = 'o', long)]
        output: PathBuf,

        #[clap(short = 's', long)]
        strip: bool,

        /// Will only run if the output file doesn't exist, or the input is newer.
        #[clap(long)]
        run_only_if_newer: bool,

        /// The input file.
        input: PathBuf,
    },

    /// Disassembles a .polkavm blob into its human-readable assembly.
    Disassemble {
        /// The output file.
        #[clap(short = 'o', long)]
        output: Option<PathBuf>,

        #[clap(short = 'f', long, value_enum, default_value_t = DisassemblyFormat::Guest)]
        format: DisassemblyFormat,

        /// The input file.
        input: PathBuf,
    },
}

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(format!($($arg)*))
    }
}

fn main() {
    env_logger::init();

    let args = Args::parse();
    let result = match args {
        Args::Link {
            output,
            input,
            strip,
            run_only_if_newer,
        } => main_link(input, output, strip, run_only_if_newer),
        Args::Disassemble { output, format, input } => main_disassemble(input, format, output),
    };

    if let Err(error) = result {
        eprintln!("ERROR: {}", error);
        std::process::exit(1);
    }
}

fn main_link(input: PathBuf, output: PathBuf, strip: bool, run_only_if_newer: bool) -> Result<(), String> {
    if run_only_if_newer {
        if let Ok(output_mtime) = std::fs::metadata(&output).and_then(|m| m.modified()) {
            if let Ok(input_mtime) = std::fs::metadata(&input).and_then(|m| m.modified()) {
                if output_mtime >= input_mtime {
                    return Ok(());
                }
            }
        }
    }

    let mut config = polkavm_linker::Config::default();
    config.set_strip(strip);

    let data = match std::fs::read(&input) {
        Ok(data) => data,
        Err(error) => {
            bail!("failed to read {input:?}: {error}");
        }
    };

    let blob = match polkavm_linker::program_from_elf(config, &data) {
        Ok(blob) => blob,
        Err(error) => {
            bail!("failed to link {input:?}: {error}");
        }
    };

    if let Err(error) = std::fs::write(&output, blob.as_bytes()) {
        bail!("failed to write the program blob to {output:?}: {error}");
    }

    Ok(())
}

fn main_disassemble(input: PathBuf, format: DisassemblyFormat, output: Option<PathBuf>) -> Result<(), String> {
    let data = match std::fs::read(&input) {
        Ok(data) => data,
        Err(error) => {
            bail!("failed to read {input:?}: {error}");
        }
    };
    let blob = match polkavm_linker::ProgramBlob::parse(&data[..]) {
        Ok(b) => b,
        Err(error) => {
            bail!("failed to parse {input:?}: {error}");
        }
    };

    let native = if matches!(format, DisassemblyFormat::Native | DisassemblyFormat::GuestAndNative) {
        if !cfg!(target_arch = "x86_64") {
            bail!("the selected disassembly format is not supported on this architecture");
        }

        let config = match polkavm::Config::from_env() {
            Ok(config) => config,
            Err(error) => bail!("failed to fetch VM configuration from the environment: {error}"),
        };

        let engine = match polkavm::Engine::new(&config) {
            Ok(engine) => engine,
            Err(error) => bail!("failed to create VM engine: {error}"),
        };

        let module = match polkavm::Module::from_blob(&engine, &Default::default(), &blob) {
            Ok(module) => module,
            Err(error) => bail!("failed to compile {input:?}: {error}"),
        };

        let code = match module.machine_code() {
            Some(code) => code.into_owned(),
            None => bail!("currently selected VM backend doesn't provide raw machine code"),
        };

        let instruction_map = match module.nth_instruction_to_code_offset_map() {
            Some(map) => map.to_vec(),
            None => bail!("currently selected VM backend doesn't provide a machine code map"),
        };

        Some((code, instruction_map))
    } else {
        None
    };

    match output {
        Some(output) => {
            let fp = match std::fs::File::create(&output) {
                Ok(fp) => fp,
                Err(error) => {
                    bail!("failed to create output file {output:?}: {error}");
                }
            };

            disassemble_into(format, &blob, native, std::io::BufWriter::new(fp))
        }
        None => {
            let stdout = std::io::stdout();
            disassemble_into(format, &blob, native, std::io::BufWriter::new(stdout))
        }
    }
}

#[derive(Default)]
struct AssemblyFormatter {
    buffer: String,
}

impl AssemblyFormatter {
    fn emit(&mut self, indent: bool, mut code: &[u8], mut position: usize, writer: &mut impl Write) -> Result<(), std::io::Error> {
        use iced_x86::Formatter;

        let mut formatter = iced_x86::NasmFormatter::new();
        formatter.options_mut().set_space_after_operand_separator(true);
        formatter.options_mut().set_hex_prefix("0x");
        formatter.options_mut().set_hex_suffix("");
        formatter.options_mut().set_uppercase_hex(false);
        formatter.options_mut().set_small_hex_numbers_in_decimal(false);
        formatter.options_mut().set_show_useless_prefixes(true);
        formatter.options_mut().set_branch_leading_zeros(false);
        formatter.options_mut().set_rip_relative_addresses(true);

        let code_origin = polkavm_common::zygote::VM_ADDR_NATIVE_CODE;
        loop {
            let mut decoder = iced_x86::Decoder::with_ip(64, code, code_origin, iced_x86::DecoderOptions::NONE);
            if !decoder.can_decode() {
                break;
            }
            let mut instruction = iced_x86::Instruction::default();
            decoder.decode_out(&mut instruction);

            if indent {
                write!(writer, "                                       ")?;
            }
            write!(writer, "{:8x}: ", position as u64 + code_origin)?;

            let start_index = (instruction.ip() - code_origin) as usize;
            let instr_bytes = &code[start_index..start_index + instruction.len()];
            let mut count = 0;
            for b in instr_bytes.iter() {
                write!(writer, "{:02x} ", b)?;
                count += 3;
            }
            while count < 34 {
                write!(writer, " ")?;
                count += 1;
            }

            self.buffer.clear();
            formatter.format(&instruction, &mut self.buffer);
            write!(writer, "{}", self.buffer)?;
            writeln!(writer)?;

            code = &code[instruction.len()..];
            position += instruction.len();
        }

        Ok(())
    }
}

fn disassemble_into(
    format: DisassemblyFormat,
    blob: &polkavm_linker::ProgramBlob,
    native: Option<(Vec<u8>, Vec<u32>)>,
    mut writer: impl Write,
) -> Result<(), String> {
    let mut instructions = Vec::new();
    for (nth_instruction, maybe_instruction) in blob.instructions().enumerate() {
        let instruction = match maybe_instruction {
            Ok(instruction) => instruction,
            Err(error) => {
                bail!("failed to parse instruction #{nth_instruction}: {error}");
            }
        };

        instructions.push(instruction);
    }

    let mut exports_for_jump_target = HashMap::new();
    for (nth_export, export) in blob.exports().enumerate() {
        let export = match export {
            Ok(export) => export,
            Err(error) => {
                bail!("failed to parse instruction export: {error}");
            }
        };

        exports_for_jump_target
            .entry(export.address())
            .or_insert_with(Vec::new)
            .push((nth_export, export));
    }

    let mut jump_table_map = HashMap::new();
    let mut jump_table = Vec::new();
    for maybe_target in blob.jump_table() {
        let target = match maybe_target {
            Ok(target) => target,
            Err(error) => {
                bail!("failed to parse the jump table: {error}");
            }
        };

        let jump_table_index = jump_table.len() + 1;
        jump_table.push(target);
        assert!(jump_table_map.insert(target, jump_table_index).is_none());
    }

    let format_jump_target = |jump_target_counter: u32| {
        if let Some(jump_table_index) = jump_table_map.get(&jump_target_counter) {
            if !matches!(format, DisassemblyFormat::DiffFriendly) {
                format!("@{jump_target_counter:x}: [@dyn {jump_table_index:x}]")
            } else {
                "@_: [_]".to_string()
            }
        } else if !matches!(format, DisassemblyFormat::DiffFriendly) {
            format!("@{jump_target_counter:x}:")
        } else {
            "@_:".to_string()
        }
    };

    let mut fmt = AssemblyFormatter::default();
    let mut last_line_program_entry = None;
    let mut last_full_name = String::new();
    let mut jump_target_counter = 0;
    let mut pending_label = true;
    for (nth_instruction, instruction) in instructions.iter().enumerate() {
        let instruction_s = if instruction.op() == polkavm_common::program::Opcode::fallthrough {
            format_jump_target(jump_target_counter + 1)
        } else {
            instruction.to_string()
        };

        let line_program = match blob.get_debug_line_program_at(nth_instruction as u32) {
            Ok(line_program) => line_program,
            Err(error) => {
                bail!("failed to parse line program: {error}");
            }
        };

        if let Some(mut line_program) = line_program {
            if last_line_program_entry != Some(line_program.entry_index()) {
                if nth_instruction != 0 {
                    if let Err(error) = writeln!(&mut writer) {
                        bail!("failed to write to output: {error}");
                    }
                }

                last_line_program_entry = Some(line_program.entry_index());
                loop {
                    let region = match line_program.run() {
                        Ok(Some(region)) => region,
                        Ok(None) => break,
                        Err(error) => {
                            bail!("failed to parse line program: {error}");
                        }
                    };

                    if region.instruction_range().contains(&(nth_instruction as u32)) {
                        let frame = region.frames().next().unwrap();
                        let full_name = match frame.full_name() {
                            Ok(full_name) => full_name,
                            Err(error) => {
                                bail!("failed to parse line program: {error}");
                            }
                        }
                        .to_string();

                        if last_full_name != full_name {
                            if let Err(error) = writeln!(&mut writer, "<{}>:", full_name) {
                                bail!("failed to write to output: {error}");
                            }

                            last_full_name = full_name;
                        }

                        break;
                    }
                }
            }
        } else {
            if !last_full_name.is_empty() {
                if let Err(error) = writeln!(&mut writer) {
                    bail!("failed to write to output: {error}");
                }
            }

            last_line_program_entry = None;
            last_full_name.clear();
        }

        if pending_label {
            pending_label = false;
            let result = if !matches!(format, DisassemblyFormat::DiffFriendly) {
                writeln!(&mut writer, "      : {}", format_jump_target(jump_target_counter))
            } else {
                writeln!(&mut writer, "    {}", format_jump_target(jump_target_counter))
            };

            if let Err(error) = result {
                bail!("failed to write to output: {error}");
            }
        }

        if matches!(format, DisassemblyFormat::DiffFriendly) {
            let mut string = instruction_s;
            if instruction.op() == polkavm_common::program::Opcode::add_imm && instruction.reg2() == polkavm_common::program::Reg::Zero {
                string = format!("{} = _", instruction.reg1());
            }

            if let Some(index) = string.find('@') {
                let length = string[index + 1..]
                    .chars()
                    .take_while(|character| character.is_ascii_digit() || matches!(character, 'a' | 'b' | 'c' | 'd' | 'e' | 'f'))
                    .count();
                string.replace_range(index + 1..index + 1 + length, "_");
            }

            if let Some(index_1) = string.find("[0x") {
                let index_2 = string[index_1..].find(']').unwrap() + index_1;
                string.replace_range(index_1..index_2 + 1, "[_]");
            }

            if let Err(error) = writeln!(&mut writer, "    {}", string) {
                bail!("failed to write to output: {error}");
            }
        } else if matches!(format, DisassemblyFormat::Guest | DisassemblyFormat::GuestAndNative) {
            if let Err(error) = writeln!(&mut writer, "{nth_instruction:6}: {instruction_s}") {
                bail!("failed to write to output: {error}");
            }
        }

        if matches!(format, DisassemblyFormat::Native | DisassemblyFormat::GuestAndNative) {
            let (code, map) = native.as_ref().unwrap();
            let code_position = map[nth_instruction] as usize;
            let next_code_position = map[nth_instruction + 1] as usize;
            let length = next_code_position - code_position;
            if length != 0 {
                let code_chunk = &code[code_position..next_code_position];
                if let Err(error) = fmt.emit(
                    matches!(format, DisassemblyFormat::GuestAndNative),
                    code_chunk,
                    code_position,
                    &mut writer,
                ) {
                    bail!("failed to write to output: {error}");
                }
            }
        }

        match instruction.op() {
            polkavm_common::program::Opcode::fallthrough
            | polkavm_common::program::Opcode::jump_and_link_register
            | polkavm_common::program::Opcode::trap
            | polkavm_common::program::Opcode::branch_less_unsigned
            | polkavm_common::program::Opcode::branch_less_signed
            | polkavm_common::program::Opcode::branch_greater_or_equal_unsigned
            | polkavm_common::program::Opcode::branch_greater_or_equal_signed
            | polkavm_common::program::Opcode::branch_eq
            | polkavm_common::program::Opcode::branch_not_eq => {
                if instruction.op() != polkavm_common::program::Opcode::fallthrough && nth_instruction + 1 != instructions.len() {
                    pending_label = true;
                }
                jump_target_counter += 1;
            }
            _ => {}
        }
    }

    if let Err(error) = writer.flush() {
        bail!("failed to write to output: {error}");
    }

    Ok(())
}
