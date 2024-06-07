use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt, vec,
};

use itertools::Itertools;
use powdr_asm_utils::{
    ast::{BinaryOpKind, UnaryOpKind},
    data_parser,
    data_storage::{store_data_objects, SingleDataValue},
    parser::parse_asm,
    reachability::{self, symbols_in_args},
    utils::{
        argument_to_escaped_symbol, argument_to_number, escape_label, expression_to_number, quote,
    },
    Architecture,
};
use powdr_number::{FieldElement, KnownField};

use crate::continuations::bootloader::{bootloader_and_shutdown_routine, bootloader_preamble};
use crate::disambiguator;
use crate::parser::RiscParser;
use crate::runtime::Runtime;
use crate::{Argument, Expression, Statement};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Register {
    value: u8,
}

impl Register {
    pub fn new(value: u8) -> Self {
        Self { value }
    }

    pub fn is_zero(&self) -> bool {
        self.value == 0
    }

    pub fn addr(&self) -> u8 {
        self.value
    }
}

impl powdr_asm_utils::ast::Register for Register {}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.value < 32 {
            // 0 indexed
            write!(f, "x{}", self.value)
        } else if self.value < 36 {
            // 1 indexed
            write!(f, "tmp{}", self.value - 31 + 1)
        } else if self.value == 36 {
            write!(f, "lr_sc_reservation")
        } else {
            // 0 indexed
            write!(f, "xtra{}", self.value - 37)
        }
    }
}

impl From<&str> for Register {
    fn from(s: &str) -> Self {
        if s.starts_with("x") {
            // 0 indexed
            let value = s[1..].parse().expect("Invalid register");
            assert!(value < 32, "Invalid register");
            Self::new(value)
        } else if s.starts_with("tmp") {
            // 1 indexed
            let value: u8 = s[3..].parse().expect("Invalid register");
            assert!(value >= 1);
            assert!(value <= 4);
            Self::new(value - 1 + 32)
        } else if s == "lr_sc_reservation" {
            Self::new(36)
        } else if s.starts_with("xtra") {
            // 0 indexed
            let value: u8 = s[4..].parse().expect("Invalid register");
            Self::new(value + 37)
        } else {
            panic!("Invalid register")
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum FunctionKind {
    HiDataRef,
    LoDataRef,
}

impl powdr_asm_utils::ast::FunctionOpKind for FunctionKind {}

impl fmt::Display for FunctionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FunctionKind::HiDataRef => write!(f, "%hi"),
            FunctionKind::LoDataRef => write!(f, "%lo"),
        }
    }
}

struct RiscvArchitecture {}

impl Architecture for RiscvArchitecture {
    fn instruction_ends_control_flow(instr: &str) -> bool {
        match instr {
            "li" | "lui" | "la" | "mv" | "add" | "addi" | "sub" | "neg" | "mul" | "mulh"
            | "mulhu" | "mulhsu" | "divu" | "remu" | "xor" | "xori" | "and" | "andi" | "or"
            | "ori" | "not" | "slli" | "sll" | "srli" | "srl" | "srai" | "seqz" | "snez"
            | "slt" | "slti" | "sltu" | "sltiu" | "sgtz" | "beq" | "beqz" | "bgeu" | "bltu"
            | "blt" | "bge" | "bltz" | "blez" | "bgtz" | "bgez" | "bne" | "bnez" | "jal"
            | "jalr" | "call" | "ecall" | "ebreak" | "lw" | "lb" | "lbu" | "lh" | "lhu" | "sw"
            | "sh" | "sb" | "nop" | "fence" | "fence.i" | "amoadd.w" | "amoadd.w.aq"
            | "amoadd.w.rl" | "amoadd.w.aqrl" | "lr.w" | "lr.w.aq" | "lr.w.rl" | "lr.w.aqrl"
            | "sc.w" | "sc.w.aq" | "sc.w.rl" | "sc.w.aqrl" => false,
            "j" | "jr" | "tail" | "ret" | "unimp" => true,
            _ => {
                panic!("Unknown instruction: {instr}");
            }
        }
    }

    fn get_references<
        'a,
        R: powdr_asm_utils::ast::Register,
        F: powdr_asm_utils::ast::FunctionOpKind,
    >(
        instr: &str,
        args: &'a [powdr_asm_utils::ast::Argument<R, F>],
    ) -> Vec<&'a str> {
        // fence arguments are not symbols, they are like reserved
        // keywords affecting the instruction behavior
        if instr.starts_with("fence") {
            Vec::new()
        } else {
            symbols_in_args(args)
        }
    }
}

/// Compiles riscv assembly to a powdr assembly file. Adds required library routines.
pub fn compile<T: FieldElement>(
    mut assemblies: BTreeMap<String, String>,
    runtime: &Runtime,
    with_bootloader: bool,
) -> String {
    // stack grows towards zero
    let stack_start = 0x10000;
    // data grows away from zero
    let data_start = 0x10100;

    assert!(assemblies
        .insert("__runtime".to_string(), runtime.global_declarations())
        .is_none());

    // TODO remove unreferenced files.
    let (mut statements, file_ids) = disambiguator::disambiguate(
        assemblies
            .into_iter()
            .map(|(name, contents)| (name, parse_asm(RiscParser::default(), &contents)))
            .collect(),
    );
    let mut data_sections = data_parser::extract_data_objects(&statements);

    // Reduce to the code that is actually reachable from main
    // (and the objects that are referred from there)
    let data_labels = reachability::filter_reachable_from::<_, _, RiscvArchitecture>(
        "__runtime_start",
        &mut statements,
        &mut data_sections,
    );

    // Replace dynamic references to code labels
    replace_dynamic_label_references(&mut statements, &data_labels);

    let mut initial_mem = Vec::new();
    let mut data_code = Vec::new();
    let data_positions =
        store_data_objects(data_sections, data_start, &mut |label, addr, value| {
            if let Some(label) = label {
                let comment = format!(" // data {label}");
                if with_bootloader && !matches!(value, SingleDataValue::LabelReference(_)) {
                    &mut initial_mem
                } else {
                    &mut data_code
                }
                .push(comment);
            }
            match value {
                SingleDataValue::Value(v) => {
                    if with_bootloader {
                        // Instead of generating the data loading code, we store it
                        // in the variable that will be used as the initial memory
                        // snapshot, committed by the bootloader.
                        initial_mem.push(format!("(0x{addr:x}, 0x{v:x})"));
                    } else {
                        // There is no bootloader to commit to memory, so we have to
                        // load it explicitly.
                        data_code.push(format!("val2 <=X= 0x{v:x};"));
                        data_code.push(format!("val1 <=X= 0x{addr:x};"));
                        data_code.push(format!("mstore 0;"));
                    }
                }
                SingleDataValue::LabelReference(sym) => {
                    // The label value is not known at this point, so we have to
                    // load it via code, irrespectively of bootloader availability.
                    //
                    // TODO should be possible without temporary
                    data_code.extend([
                        format!("load_label({});", escape_label(sym)),
                        format!("val2 <=X= tmp1;"),
                        format!("val1 <=X= 0x{addr:x};"),
                        format!("mstore 0;"),
                    ]);
                }
                SingleDataValue::Offset(_, _) => {
                    unimplemented!();
                    /*
                    object_code.push(format!("addr <=X= 0x{pos:x};"));

                    I think this solution should be fine but hard to say without
                    an actual code snippet that uses it.

                    // TODO should be possible without temporary
                    object_code.extend([
                        format!("tmp1 <== load_label({});", escape_label(a)),
                        format!("tmp2 <== load_label({});", escape_label(b)),
                        // TODO check if registers match
                        "mstore wrap(tmp1 - tmp2);".to_string(),
                    ]);
                    */
                }
            }
        });

    let submachines_init = runtime.submachines_init();
    let bootloader_and_shutdown_routine_lines = if with_bootloader {
        let bootloader_and_shutdown_routine = bootloader_and_shutdown_routine(&submachines_init);
        log::debug!("Adding Bootloader:\n{}", bootloader_and_shutdown_routine);
        bootloader_and_shutdown_routine
            .split('\n')
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
    } else {
        submachines_init
    };

    let mut program: Vec<String> = file_ids
        .into_iter()
        .map(|(id, dir, file)| format!(".debug file {id} {} {};", quote(&dir), quote(&file)))
        .chain(bootloader_and_shutdown_routine_lines)
        .collect();
    if !data_code.is_empty() {
        program.push("set_reg 1, pc + 2;".to_string());
        program.push("jump __data_init;".to_string());
    }
    program.extend([
        format!("// Set stack pointer\nx2 <=X= {stack_start};"),
        "set_reg 2, x2;".to_string(),
        "set_reg 1, pc + 2;".to_string(),
        "jump __runtime_start;".to_string(),
        "return;".to_string(), // This is not "riscv ret", but "return from powdr asm function".
    ]);
    program.extend(
        substitute_symbols_with_values(statements, &data_positions)
            .into_iter()
            .flat_map(process_statement),
    );
    if !data_code.is_empty() {
        program.extend(
        ["// This is the data initialization routine.\n__data_init:".to_string()].into_iter()
        .chain(data_code)
        .chain([
            "// This is the end of the data initialization routine.\nval1 <== get_reg(1);\njump_dyn;"
                .to_string(),
        ]));
    }
    program.extend(runtime.ecall_handler());

    // The program ROM needs to fit the degree, so we use the next power of 2.
    let degree = program.len().ilog2() + 1;
    let degree = std::cmp::max(degree, 18);
    log::info!("Inferred degree 2^{degree}");

    // In practice, these are the lengths of single proofs that we want to support.
    // Reasoning:
    // - 18: is the lower bound for the Binary and Shift machines.
    // - 20: revm's ROM does not fit in 2^19.
    // - >20: may be needed in the future.
    // This is an assert for now, but could be a compiler warning or error.
    // TODO note that if the degree is higher than 18 we might need mux machines for Binary and
    // Shift.
    assert!((18..=20).contains(&degree));
    let degree = 1 << degree;

    riscv_machine(
        runtime,
        degree,
        &preamble::<T>(runtime, with_bootloader),
        initial_mem,
        program,
    )
}

/// Replace certain patterns of references to code labels by
/// special instructions. We ignore any references to data objects
/// because they will be handled differently.
fn replace_dynamic_label_references(statements: &mut Vec<Statement>, data_labels: &HashSet<&str>) {
    /*
    Find patterns of the form
    lui	a0, %hi(LABEL)
    addi	s10, a0, %lo(LABEL)
    -
    turn this into the pseudoinstruction
    li s10, LABEL
    which is then turned into

    s10 <== load_label(LABEL)

    It gets complicated by the fact that sometimes, labels
    and debugging directives occur between the two statements
    matching that pattern...
    */
    let instruction_indices = statements
        .iter()
        .enumerate()
        .filter_map(|(i, s)| match s {
            Statement::Instruction(_, _) => Some(i),
            _ => None,
        })
        .collect::<Vec<_>>();

    let mut to_delete = BTreeSet::default();
    for (i1, i2) in instruction_indices.into_iter().tuple_windows() {
        if let Some(r) =
            replace_dynamic_label_reference(&statements[i1], &statements[i2], data_labels)
        {
            to_delete.insert(i1);
            statements[i2] = r;
        }
    }

    let mut i = 0;
    statements.retain(|_| (!to_delete.contains(&i), i += 1).0);
}

fn replace_dynamic_label_reference(
    s1: &Statement,
    s2: &Statement,
    data_labels: &HashSet<&str>,
) -> Option<Statement> {
    let Statement::Instruction(instr1, args1) = s1 else {
        return None;
    };
    let Statement::Instruction(instr2, args2) = s2 else {
        return None;
    };
    if instr1.as_str() != "lui" || instr2.as_str() != "addi" {
        return None;
    };
    let [Argument::Register(r1), Argument::Expression(Expression::FunctionOp(FunctionKind::HiDataRef, expr1))] =
        &args1[..]
    else {
        return None;
    };
    // Maybe should try to reduce expr1 and expr2 before comparing deciding it is a pure symbol?
    let Expression::Symbol(label1) = expr1.as_ref() else {
        return None;
    };
    let [Argument::Register(r2), Argument::Register(r3), Argument::Expression(Expression::FunctionOp(FunctionKind::LoDataRef, expr2))] =
        &args2[..]
    else {
        return None;
    };
    let Expression::Symbol(label2) = expr2.as_ref() else {
        return None;
    };
    if r1 != r3 || label1 != label2 || data_labels.contains(label1.as_str()) {
        return None;
    }
    Some(Statement::Instruction(
        "li".to_string(),
        vec![
            Argument::Register(*r2),
            Argument::Expression(Expression::Symbol(label1.clone())),
        ],
    ))
}

fn substitute_symbols_with_values(
    mut statements: Vec<Statement>,
    data_positions: &BTreeMap<String, u32>,
) -> Vec<Statement> {
    for s in &mut statements {
        let Statement::Instruction(_name, args) = s else {
            continue;
        };
        for arg in args {
            arg.post_visit_expressions_mut(&mut |expression| match expression {
                Expression::Number(_) => {}
                Expression::Symbol(symb) => {
                    if let Some(pos) = data_positions.get(symb) {
                        *expression = Expression::Number(*pos as i64)
                    }
                }
                Expression::UnaryOp(op, subexpr) => {
                    if let Expression::Number(num) = subexpr.as_ref() {
                        let result = match op {
                            UnaryOpKind::BitwiseNot => !num,
                            UnaryOpKind::Negation => -num,
                        };
                        *expression = Expression::Number(result);
                    };
                }
                Expression::BinaryOp(op, subexprs) => {
                    if let (Expression::Number(a), Expression::Number(b)) =
                        (&subexprs[0], &subexprs[1])
                    {
                        let result = match op {
                            BinaryOpKind::Or => a | b,
                            BinaryOpKind::Xor => a ^ b,
                            BinaryOpKind::And => a & b,
                            BinaryOpKind::LeftShift => a << b,
                            BinaryOpKind::RightShift => a >> b,
                            BinaryOpKind::Add => a + b,
                            BinaryOpKind::Sub => a - b,
                            BinaryOpKind::Mul => a * b,
                            BinaryOpKind::Div => a / b,
                            BinaryOpKind::Mod => a % b,
                        };
                        *expression = Expression::Number(result);
                    }
                }
                Expression::FunctionOp(op, subexpr) => {
                    if let Expression::Number(num) = subexpr.as_ref() {
                        let result = match op {
                            FunctionKind::HiDataRef => num >> 12,
                            FunctionKind::LoDataRef => num & 0xfff,
                        };
                        *expression = Expression::Number(result);
                    };
                }
            });
        }
    }
    statements
}

fn riscv_machine(
    runtime: &Runtime,
    degree: u64,
    preamble: &str,
    initial_memory: Vec<String>,
    program: Vec<String>,
) -> String {
    format!(
        r#"
{}
machine Main with degree: {degree} {{
{}

{}

let initial_memory: (fe, fe)[] = [
{}
];

    function main {{
{}
    }}
}}    
"#,
        runtime.submachines_import(),
        runtime.submachines_declare(),
        preamble,
        initial_memory
            .into_iter()
            .format_with(",\n", |line, f| f(&format_args!("\t\t{line}"))),
        program
            .into_iter()
            .format_with("\n", |line, f| f(&format_args!("\t\t{line}"))),
    )
}

fn preamble<T: FieldElement>(runtime: &Runtime, with_bootloader: bool) -> String {
    let bootloader_preamble_if_included = if with_bootloader {
        bootloader_preamble()
    } else {
        "".to_string()
    };

    for machine in ["binary", "shift"] {
        assert!(
            runtime.has_submachine(machine),
            "RISC-V machine requires the `{machine}` submachine"
        );
    }

    let mul_instruction = mul_instruction::<T>(runtime);

    r#"
    reg pc[@pc];
    reg X[<=];
    reg Y[<=];
    reg Z[<=];
    reg W[<=];
    reg tmp1;
    reg tmp2;
    reg tmp3;
    reg tmp4;
    reg lr_sc_reservation;
"#
        .to_string()
        // risc-v x* registers
        + &(0..32)
            .map(|i| format!("\t\treg x{i};\n"))
            .join("")
        // runtime extra registers
        + &runtime
            .submachines_extra_registers()
            .into_iter()
            .map(|s| format!("\t\t{s}\n"))
            .join("")
        + &bootloader_preamble_if_included
        + &memory(with_bootloader)
        + r#"
    // ============== Constraint on x0 =======================

    x0 = 0;

    // ============== iszero check for X =======================
    let XIsZero = std::utils::is_zero(X);

    // ============== control-flow instructions ==============

    instr load_label l: label { val3' = l }

    instr jump l: label { pc' = l, val3' = pc + 1}
    instr jump_dyn { pc' = val1, val3' = pc + 1}

    instr branch_if_nonzero l: label { XXIsZero = 1 - XX * XX_inv, XX = val1 - val2, pc' = (1 - XXIsZero) * l + XXIsZero * (pc + 1) }
    instr branch_if_zero X, l: label { pc' = XIsZero * l + (1 - XIsZero) * (pc + 1) }

    // Skips Y instructions if X is zero
    instr skip_if_zero X, Y { pc' = pc + 1 + (XIsZero * Y) }

    // input X is required to be the difference of two 32-bit unsigend values.
    // i.e. -2**32 < X < 2**32
    instr branch_if_positive X, l: label {
        X + 2**32 - 1 = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000 + wrap_bit * 2**32,
        pc' = wrap_bit * l + (1 - wrap_bit) * (pc + 1)
    }
    // input X is required to be the difference of two 32-bit unsigend values.
    // i.e. -2**32 < X < 2**32
    instr is_positive X, Y {
        (val1 * Y + X + val2) + 2**32 - 1 = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000 + wrap_bit * 2**32,
        val3' = wrap_bit
    }

    // ======aaaaaaaaaaaa=========

    // Wraps a value in Y to 32 bits.
    // Requires 0 <= Y < 2**33
    // These are the old `wrap` instruction.
    instr add_new { val1 + val2 = val3' + wrap_bit * 2**32, val3' = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000 }
    instr add_new_2 Y { val1 + Y = val3' + wrap_bit * 2**32, val3' = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000 }

    // Requires -2**32 <= Y < 2**32
    // These are the old `wrap_signed` instruction.
    instr add_new_signed { (val1 - val2) + 2**32 = val3' + wrap_bit * 2**32, val3' = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000 }
    instr add_new_signed_2 Y { (-val1 + Y) + 2**32 = val3' + wrap_bit * 2**32, val3' = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000 }

    // =====bbbbbbbbb======

    // ================= logical instructions =================

    instr is_equal_zero X { val3' = XIsZero }
    instr is_not_equal_zero X { val3' = 1 - XIsZero }

    // ================= submachine instructions =================
"# + &runtime
        .submachines_instructions()
        .into_iter()
        .map(|s| format!("    {s}"))
        .join("\n")
        + r#"
    col fixed bytes(i) { i & 0xff };
    col witness X_b1;
    col witness X_b2;
    col witness X_b3;
    col witness X_b4;
    { X_b1 } in { bytes };
    { X_b2 } in { bytes };
    { X_b3 } in { bytes };
    { X_b4 } in { bytes };
    col witness wrap_bit;
    wrap_bit * (1 - wrap_bit) = 0;

    // Input is a 32 bit unsigned number. We check bit 7 and set all higher bits to that value.
    instr sign_extend_byte {
        // wrap_bit is used as sign_bit here.
        val1 = Y_7bit + wrap_bit * 0x80 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000,
        val3' = Y_7bit + wrap_bit * 0xffffff80
    }
    col fixed seven_bit(i) { i & 0x7f };
    col witness Y_7bit;
    { Y_7bit } in { seven_bit };

    // Input is a 32 bit unsigned number. We check bit 15 and set all higher bits to that value.
    instr sign_extend_16_bits {
        Y_15bit = X_b1 + Y_7bit * 0x100,

        // wrap_bit is used as sign_bit here.
        val1 = Y_15bit + wrap_bit * 0x8000 + X_b3 * 0x10000 + X_b4 * 0x1000000,
        val3' = Y_15bit + wrap_bit * 0xffff8000
    }
    col witness Y_15bit;

    // Input is a 32 but unsigned number (0 <= Y < 2**32) interpreted as a two's complement numbers.
    // Returns a signed number (-2**31 <= X < 2**31).
    instr to_signed {
        // wrap_bit is used as sign_bit here.
        val1 = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + Y_7bit * 0x1000000 + wrap_bit * 0x80000000,
        val3' = val1 - wrap_bit * 0x100000000
    }

    // ======================= assertions =========================

    instr fail { 1 = 0 }

    // Removes up to 16 bits beyond 32
    // TODO is this really safe?
    // Y = val1 * val2
    // X = val3'
    instr wrap16 { (val1 * val2) = Y_b5 * 2**32 + Y_b6 * 2**40 + val3', val3' = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000 }
    col witness Y_b5;
    col witness Y_b6;
    col witness Y_b7;
    col witness Y_b8;
    { Y_b5 } in { bytes };
    { Y_b6 } in { bytes };
    { Y_b7 } in { bytes };
    { Y_b8 } in { bytes };

    col witness REM_b1;
    col witness REM_b2;
    col witness REM_b3;
    col witness REM_b4;
    { REM_b1 } in { bytes };
    { REM_b2 } in { bytes };
    { REM_b3 } in { bytes };
    { REM_b4 } in { bytes };

    // implements Z = Y / X and W = Y % X.
    instr divremu Y, X {
        // main division algorithm:
        // Y is the known dividend
        // X is the known divisor
        // val3 is the unknown quotient
        // val4 is the unknown remainder
        // if X is zero, remainder is set to dividend, as per RISC-V specification:
        X * val3' + val4' = Y,

        // remainder >= 0:
        val4' = REM_b1 + REM_b2 * 0x100 + REM_b3 * 0x10000 + REM_b4 * 0x1000000,

        // remainder < divisor, conditioned to X not being 0:
        (1 - XIsZero) * (X - val4' - 1 - Y_b5 - Y_b6 * 0x100 - Y_b7 * 0x10000 - Y_b8 * 0x1000000) = 0,

        // in case X is zero, we set quotient according to RISC-V specification
        XIsZero * (val3' - 0xffffffff) = 0,

        // quotient is 32 bits:
        val3' = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000
    }
"# + mul_instruction
}

fn mul_instruction<T: FieldElement>(runtime: &Runtime) -> &'static str {
    match T::known_field().expect("Unknown field!") {
        KnownField::Bn254Field => {
            // The BN254 field can fit any 64-bit number, so we can naively de-compose
            // Z * W into 8 bytes and put them together to get the upper and lower word.
            r#"
    // Multiply two 32-bits unsigned, return the upper and lower unsigned 32-bit
    // halves of the result.
    // X is the lower half (least significant bits)
    // Y is the higher half (most significant bits)
    instr mul {
        val1 * val2 = val3' + val4' * 2**32,
        val3' = X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000,
        val4' = Y_b5 + Y_b6 * 0x100 + Y_b7 * 0x10000 + Y_b8 * 0x1000000
    }
"#
        }
        KnownField::GoldilocksField => {
            assert!(
                runtime.has_submachine("split_gl"),
                "RISC-V machine with the goldilocks field requires the `split_gl` submachine"
            );
            // The Goldilocks field cannot fit some 64-bit numbers, so we have to use
            // the split machine. Note that it can fit a product of two 32-bit numbers.
            r#"
    // Multiply two 32-bits unsigned, return the upper and lower unsigned 32-bit
    // halves of the result.
    // X is the lower half (least significant bits)
    // Y is the higher half (most significant bits)
    instr mul ~ split_gl.split val1 * val2 -> val3', val4';
"#
        }
    }
}

fn memory(with_bootloader: bool) -> String {
    // There are subtle differences between the memory machines with and without continuations:
    // - There is an extra `mstore_bootloader` instruction. For the most part, it behaves just
    //   like `mstore`.
    // - When `m_change` is true, the `m_is_bootloader_write` has to be true in the next row.
    // - The `(1 - m_is_write') * m_change * m_value' = 0` constraint is removed, as we no longer can
    //   have a read as the first operation on a new address.
    // - The `(1 - m_change) * LAST = 0` constraint is replaced with
    //   `LAST * (1 - m_change) * (m_addr + 1) = 0`. This allows for a valid assignment in the case
    //   where there is no memory operation in the entire chunk: The address can be set to -1 (which
    //   cannot be represented in 32 bits, hence there is can't be an actual memory operation
    //   associated with it). In that case, `m_change` can be 0 everywhere.
    let bootloader_specific_parts = if with_bootloader {
        r#"
    // Memory operation flags: If none is active, it's a read.
    col witness m_is_write;
    col witness m_is_bootloader_write;
    std::utils::force_bool(m_is_write);
    std::utils::force_bool(m_is_bootloader_write);

    // Selectors
    col witness m_selector_read;
    col witness m_selector_write;
    col witness m_selector_bootloader_write;
    std::utils::force_bool(m_selector_read);
    std::utils::force_bool(m_selector_write);
    std::utils::force_bool(m_selector_bootloader_write);

    // No selector active -> no write
    (1 - m_selector_read - m_selector_write - m_selector_bootloader_write) * m_is_write = 0;
    (1 - m_selector_read - m_selector_write - m_selector_bootloader_write) * m_is_bootloader_write = 0;

    // The first operation of a new address has to be a bootloader write
    m_change * (1 - m_is_bootloader_write') = 0;

    // m_change has to be 1 in the last row, so that the above constraint is triggered.
    // An exception to this when the last address is -1, which is only possible if there is
    // no memory operation in the entire chunk (because addresses are 32 bit unsigned).
    // This exception is necessary so that there can be valid assignment in this case.
    pol m_change_or_no_memory_operations = (1 - m_change) * (m_addr + 1);
    LAST * m_change_or_no_memory_operations = 0;

    // If the next line is a read and we stay at the same address, then the
    // value cannot change.
    (1 - m_is_write' - m_is_bootloader_write') * (1 - m_change) * (m_value' - m_value) = 0;

    col operation_id = m_is_write + 2 * m_is_bootloader_write;

    /// Like mstore, but setting the m_is_bootloader_write flag.
    instr mstore_bootloader Y {
        { 2, X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000, STEP, val2 } is m_selector_bootloader_write { operation_id, m_addr, m_step, m_value },
        // Wrap the addr value
        val1 + Y = (X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000) + wrap_bit * 2**32
    }
"#
    } else {
        r#"
    // Memory operation flags: If none is active, it's a read.
    col witness m_is_write;
    std::utils::force_bool(m_is_write);

    // Selectors
    col witness m_selector_read;
    col witness m_selector_write;
    std::utils::force_bool(m_selector_read);
    std::utils::force_bool(m_selector_write);

    // No selector active -> no write
    (1 - m_selector_read - m_selector_write) * m_is_write = 0;
    
    col operation_id = m_is_write;

    // If the next line is a not a write and we have an address change,
    // then the value is zero.
    (1 - m_is_write') * m_change * m_value' = 0;

    // m_change has to be 1 in the last row, so that a first read on row zero is constrained to return 0
    (1 - m_change) * LAST = 0;

    // If the next line is a read and we stay at the same address, then the
    // value cannot change.
    (1 - m_is_write') * (1 - m_change) * (m_value' - m_value) = 0;
"#
    };

    r#"

    // =============== Register memory =======================
    std::machines::memory::Memory regs;
    instr get_reg X -> Y ~ regs.mload X, STEP -> Y;
    instr set_reg X, Y -> ~ regs.mstore X, STEP, Y ->;
    reg val1;
    reg val2;
    reg val3;
    reg val4;

    col witness XX, XX_inv, XXIsZero;
    std::utils::force_bool(XXIsZero);
    XXIsZero * XX = 0;

    // HACK: This constraint cannot be active globally, because when
    // XX is not constrained, witgen will try to set XX, XX_inv and XXIsZero
    // to zero, which fails this constraint. Therefore, we have to activate
    // constrained whenever XXIsZero is used.
    // XXIsZero = 1 - XX * XX_inv

    // =============== read-write memory =======================
    // Read-write memory. Columns are sorted by m_addr and
    // then by m_step. m_change is 1 if and only if m_addr changes
    // in the next row.
    col witness m_addr;
    col witness m_step;
    col witness m_change;
    col witness m_value;
"#
    .to_string()
        + bootloader_specific_parts
        + r#"
    col witness m_diff_lower;
    col witness m_diff_upper;

    col fixed FIRST = [1] + [0]*;
    let LAST = FIRST';
    col fixed STEP(i) { i };
    col fixed BIT16(i) { i & 0xffff };

    {m_diff_lower} in {BIT16};
    {m_diff_upper} in {BIT16};

    std::utils::force_bool(m_change);

    // if m_change is zero, m_addr has to stay the same.
    (m_addr' - m_addr) * (1 - m_change) = 0;

    // Except for the last row, if m_change is 1, then m_addr has to increase,
    // if it is zero, m_step has to increase.
    // `m_diff_upper * 2**16 + m_diff_lower` has to be equal to the difference **minus one**.
    // Since we know that both m_addr and m_step can only be 32-Bit, this enforces that
    // the values are strictly increasing.
    col diff = (m_change * (m_addr' - m_addr) + (1 - m_change) * (m_step' - m_step));
    (1 - LAST) * (diff - 1 - m_diff_upper * 2**16 - m_diff_lower) = 0;

    // ============== memory instructions ==============

    let up_to_three: col = |i| i % 4;
    let six_bits: col = |i| i % 2**6;
    /// Loads one word from an address Y, where Y can be between 0 and 2**33 (sic!),
    /// wraps the address to 32 bits and rounds it down to the next multiple of 4.
    /// Returns the loaded word and the remainder of the division by 4.
    col witness please;
    col witness please2;
    // TODO FIXXXXXXXX
    instr mload Y {
        val3' = please,
        val4' = please2,
        // Z * (Z - 1) * (Z - 2) * (Z - 3) = 0,
        { please2 } in { up_to_three },
        val1 + Y = wrap_bit * 2**32 + X_b4 * 0x1000000 + X_b3 * 0x10000 + X_b2 * 0x100 + X_b1 * 4 + please2,
        { X_b1 } in { six_bits },
        {
            0,
            X_b4 * 0x1000000 + X_b3 * 0x10000 + X_b2 * 0x100 + X_b1 * 4,
            STEP,
            please
        } is m_selector_read { operation_id, m_addr, m_step, m_value }
        // If we could access the shift machine here, we
        // could even do the following to complete the mload:
        // { W, X, Z} in { shr.value, shr.amount, shr.amount}
    }

    /// Stores Z at address Y % 2**32. Y can be between 0 and 2**33.
    /// val1 should be a multiple of 4, but this instruction does not enforce it.
    instr mstore Y {
        { 1, X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000, STEP, val2 } is m_selector_write { operation_id, m_addr, m_step, m_value },
        // Wrap the addr value
        val1 + Y = (X_b1 + X_b2 * 0x100 + X_b3 * 0x10000 + X_b4 * 0x1000000) + wrap_bit * 2**32
    }
    "#
}

fn process_statement(s: Statement) -> Vec<String> {
    match &s {
        Statement::Label(l) => vec![format!("{}:", escape_label(l))],
        Statement::Directive(directive, args) => match (directive.as_str(), &args[..]) {
            (
                ".loc",
                [Argument::Expression(Expression::Number(file)), Argument::Expression(Expression::Number(line)), Argument::Expression(Expression::Number(column)), ..],
            ) => {
                vec![format!("  .debug loc {file} {line} {column};")]
            }
            (".file", _) => {
                // We ignore ".file" directives because they have been extracted to the top.
                vec![]
            }
            (".size", _) => {
                // We ignore ".size" directives
                vec![]
            }
            _ if directive.starts_with(".cfi_") => vec![],
            _ => panic!(
                "Leftover directive in code: {directive} {}",
                args.iter().format(", ")
            ),
        },
        Statement::Instruction(instr, args) => {
            let stmt_str = format!("{s}");
            // remove indentation and trailing newline
            let stmt_str = &stmt_str[2..(stmt_str.len() - 1)];
            let mut ret = vec![format!("  .debug insn \"{stmt_str}\";")];
            let processed_instr = match process_instruction(instr, &args[..]) {
                Ok(s) => s,
                Err(e) => panic!("Failed to process instruction '{instr}'. {e}"),
            };
            ret.extend(processed_instr.into_iter().map(|s| "  ".to_string() + &s));
            ret
        }
    }
}

trait Args {
    type Error;

    fn l(&self) -> Result<String, Self::Error>;
    fn r(&self) -> Result<Register, Self::Error>;
    fn rri(&self) -> Result<(Register, Register, u32), Self::Error>;
    fn rrr(&self) -> Result<(Register, Register, Register), Self::Error>;
    fn ri(&self) -> Result<(Register, u32), Self::Error>;
    fn rr(&self) -> Result<(Register, Register), Self::Error>;
    fn rrl(&self) -> Result<(Register, Register, String), Self::Error>;
    fn rl(&self) -> Result<(Register, String), Self::Error>;
    fn rro(&self) -> Result<(Register, Register, u32), Self::Error>;
    fn rrro(&self) -> Result<(Register, Register, Register, u32), Self::Error>;
    fn empty(&self) -> Result<(), Self::Error>;
}

impl Args for [Argument] {
    type Error = &'static str;

    fn l(&self) -> Result<String, &'static str> {
        const ERR: &str = "Expected: label";
        match self {
            [l] => Ok(argument_to_escaped_symbol(l).ok_or(ERR)?),
            _ => Err(ERR),
        }
    }

    fn r(&self) -> Result<Register, &'static str> {
        match self {
            [Argument::Register(r1)] => Ok(*r1),
            _ => Err("Expected: register"),
        }
    }

    fn rri(&self) -> Result<(Register, Register, u32), &'static str> {
        const ERR: &str = "Expected: register, register, immediate";
        match self {
            [Argument::Register(r1), Argument::Register(r2), n] => {
                Ok((*r1, *r2, argument_to_number(n).ok_or(ERR)?))
            }
            _ => Err(ERR),
        }
    }

    fn rrr(&self) -> Result<(Register, Register, Register), &'static str> {
        match self {
            [Argument::Register(r1), Argument::Register(r2), Argument::Register(r3)] => {
                Ok((*r1, *r2, *r3))
            }
            _ => Err("Expected: register, register, register"),
        }
    }

    fn ri(&self) -> Result<(Register, u32), &'static str> {
        const ERR: &str = "Expected: register, immediate";
        match self {
            [Argument::Register(r1), n] => Ok((*r1, argument_to_number(n).ok_or(ERR)?)),
            _ => Err(ERR),
        }
    }

    fn rr(&self) -> Result<(Register, Register), &'static str> {
        match self {
            [Argument::Register(r1), Argument::Register(r2)] => Ok((*r1, *r2)),
            _ => Err("Expected: register, register"),
        }
    }

    fn rrl(&self) -> Result<(Register, Register, String), &'static str> {
        const ERR: &str = "Expected: register, register, label";
        match self {
            [Argument::Register(r1), Argument::Register(r2), l] => {
                Ok((*r1, *r2, argument_to_escaped_symbol(l).ok_or(ERR)?))
            }
            _ => Err(ERR),
        }
    }

    fn rl(&self) -> Result<(Register, String), &'static str> {
        const ERR: &str = "Expected: register, label";
        match self {
            [Argument::Register(r1), l] => Ok((*r1, argument_to_escaped_symbol(l).ok_or(ERR)?)),
            _ => Err(ERR),
        }
    }

    fn rro(&self) -> Result<(Register, Register, u32), &'static str> {
        if let [Argument::Register(r1), Argument::RegOffset(off, r2)] = self {
            if let Some(off) = expression_to_number(off.as_ref().unwrap_or(&Expression::Number(0)))
            {
                return Ok((*r1, *r2, off));
            }
        }
        if let [Argument::Register(r1), Argument::Expression(off)] = self {
            if let Some(off) = expression_to_number(off) {
                // If the register is not specified, it defaults to x0
                return Ok((*r1, Register::new(0), off));
            }
        }

        Err("Expected: register, offset(register)")
    }

    fn rrro(&self) -> Result<(Register, Register, Register, u32), &'static str> {
        if let [Argument::Register(r1), Argument::Register(r2), Argument::RegOffset(off, r3)] = self
        {
            if let Some(off) = expression_to_number(off.as_ref().unwrap_or(&Expression::Number(0)))
            {
                return Ok((*r1, *r2, *r3, off));
            }
        }
        if let [Argument::Register(r1), Argument::Register(r2), Argument::Expression(off)] = self {
            if let Some(off) = expression_to_number(off) {
                // If the register is not specified, it defaults to x0
                return Ok((*r1, *r2, Register::new(0), off));
            }
        }
        Err("Expected: register, register, offset(register)")
    }

    fn empty(&self) -> Result<(), &'static str> {
        match self {
            [] => Ok(()),
            _ => Err("Expected: no arguments"),
        }
    }
}

fn only_if_no_write_to_zero_val3(statement: String, reg: Register) -> Vec<String> {
    only_if_no_write_to_zero_vec_val3(vec![statement], reg)
}

fn only_if_no_write_to_zero_val4(statement: String, reg: Register) -> Vec<String> {
    only_if_no_write_to_zero_vec_val4(vec![statement], reg)
}

fn only_if_no_write_to_zero_vec_val3(statements: Vec<String>, reg: Register) -> Vec<String> {
    if reg.is_zero() {
        vec![]
    } else {
        statements
            .into_iter()
            .chain([format!("set_reg {}, val3;", reg.addr())])
            .collect()
    }
}

fn only_if_no_write_to_zero_vec_val4(statements: Vec<String>, reg: Register) -> Vec<String> {
    if reg.is_zero() {
        vec![]
    } else {
        statements
            .into_iter()
            .chain([format!("set_reg {}, val4;", reg.addr())])
            .collect()
    }
}

fn read_args(input_regs: Vec<Register>) -> Vec<String> {
    input_regs
        .into_iter()
        .enumerate()
        .flat_map(|(i, r)| {
            [
                format!("{} <== get_reg({});", r, r.addr()),
                format!("val{} <== get_reg({});", i + 1, r.addr()),
            ]
        })
        .collect()
}

fn name_to_register(name: &str) -> Option<Register> {
    if name.starts_with("x") {
        Some(Register::from(name))
    } else {
        None
    }
}

/// Push register into the stack

pub fn push_register(name: &str) -> Vec<String> {
    assert!(name.starts_with("x"), "Only x registers are supported");
    let mut statements = vec![];

    if let Some(reg) = name_to_register(name) {
        statements.push(format!("val2 <== get_reg({});", reg.addr()));
    } else {
        panic!();
    }

    [
        statements,
        vec![
            "val1 <== get_reg(2);".to_string(),
            "add_new_2 -4;".to_string(),
            "set_reg 2, val3;".to_string(),
            "val1 <== get_reg(2);".to_string(),
            format!("mstore 0;"),
        ],
    ]
    .concat()
}

/// Pop register from the stack
pub fn pop_register(name: &str) -> Vec<String> {
    assert!(name.starts_with("x"), "Only x registers are supported");
    let mut instructions = vec![
        "val1 <== get_reg(2);".to_string(),
        "mload 0;".to_string(),
        "val2 <=X= val3;".to_string(),
        "add_new_2 4;".to_string(),
        "set_reg 2, val3;".to_string(),
    ];
    if let Some(reg) = name_to_register(name) {
        instructions.push(format!("set_reg {}, val2;", reg.addr()));
    } else {
        panic!();
    }
    instructions
}

fn process_instruction<A: Args + ?Sized + std::fmt::Debug>(
    instr: &str,
    args: &A,
) -> Result<Vec<String>, A::Error> {
    log::debug!("Processing instruction: {instr}");
    log::debug!("      Arguments: {:?}", args);
    let statements = match instr {
        // load/store registers
        "li" | "la" => {
            // The difference between "li" and "la" in RISC-V is that the former
            // is for loading values as is, and the later is for loading PC
            // relative values. But since we work on a higher abstraction level,
            // for us they are the same thing.
            if let Ok((rd, label)) = args.rl() {
                only_if_no_write_to_zero_val3(format!("load_label({label});"), rd)
            } else {
                let (rd, imm) = args.ri()?;
                only_if_no_write_to_zero_val3(format!("val3 <=X= {imm};"), rd)
            }
        }
        // TODO check if it is OK to clear the lower order bits
        "lui" => {
            let (rd, imm) = args.ri()?;
            only_if_no_write_to_zero_val3(format!("val3 <=X= {};", imm << 12), rd)
        }
        "mv" => {
            let (rd, rs) = args.rr()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("val3 <=X= val1;"),
                    rd,
                ))
                .collect()
        }

        // Arithmetic
        "add" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(format!("add_new;"), rd))
                .collect()
        }
        "addi" => {
            let (rd, rs, imm) = args.rri()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("add_new_2({imm});"),
                    rd,
                ))
                .collect()
        }
        "sub" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("add_new_signed;"),
                    rd,
                ))
                .collect()
        }
        "neg" => {
            let (rd, r1) = args.rr()?;
            read_args(vec![r1])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("add_new_signed_2(0);"),
                    rd,
                ))
                .collect()
        }
        "mul" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(format!("mul;"), rd))
                .collect()
        }
        "mulhu" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val4(format!("mul;"), rd))
                .collect()
        }
        "mulh" => {
            let (rd, r1, r2) = args.rrr()?;
            // val1 = r1, val2 = r2
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        format!("to_signed;"),
                        format!("tmp1 <=X= val3;"),
                        format!("val1 <== get_reg({});", r2.addr()),
                        format!("to_signed;"),
                        format!("tmp2 <=X= val3;"),
                        // tmp3 is 1 if tmp1 is non-negative
                        format!("val1 <=X= tmp1;"),
                        format!("val2 <== get_reg(0);"),
                        "is_positive 1, 1;".into(),
                        format!("tmp3 <=X= val3;"),
                        // tmp4 is 1 if tmp2 is non-negative
                        format!("val1 <=X= tmp2;"),
                        format!("val2 <== get_reg(0);"),
                        "is_positive 1, 1;".into(),
                        format!("tmp4 <=X= val3;"),
                        // If tmp1 is negative, convert to positive
                        "skip_if_zero 0, tmp3;".into(),
                        "tmp1 <=X= 0 - tmp1;".into(),
                        // If tmp2 is negative, convert to positive
                        "skip_if_zero 0, tmp4;".into(),
                        "tmp2 <=X= 0 - tmp2;".into(),
                        "val1 <=X= tmp1;".into(),
                        "val2 <=X= tmp2;".into(),
                        "mul;".into(),
                        "tmp1 <=X= val3;".into(),
                        format!("set_reg {}, val4;", rd.addr()),
                        // Determine the sign of the result based on the signs of tmp1 and tmp2
                        "is_not_equal_zero tmp3 - tmp4;".into(),
                        "tmp3 <=X= val3;".into(),
                        // If the result should be negative, convert back to negative
                        "skip_if_zero tmp3, 5;".into(),
                        "is_equal_zero tmp1;".into(),
                        "tmp1 <=X= val3;".into(),
                        format!("val1 <== get_reg({});", rd.addr()),
                        format!("add_new_signed_2(- 1 + tmp1);"),
                    ],
                    rd,
                ))
                .collect()
        }
        "mulhsu" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        "to_signed;".into(),
                        "tmp1 <=X= val3;".into(),
                        // tmp2 is 1 if tmp1 is non-negative
                        format!("val1 <=X= tmp1;"),
                        format!("val2 <== get_reg(0);"),
                        "is_positive 1, 1;".into(),
                        format!("tmp2 <=X= val3;"),
                        // If negative, convert to positive
                        "skip_if_zero 0, tmp2;".into(),
                        "tmp1 <=X= 0 - tmp1;".into(),
                        "val1 <=X= tmp1;".into(),
                        format!("val2 <== get_reg({});", r2.addr()),
                        format!("mul;"),
                        "tmp1 <=X= val3;".into(),
                        format!("set_reg {}, val4;", rd.addr()),
                        // If was negative before, convert back to negative
                        "skip_if_zero (1-tmp2), 5;".into(),
                        "is_equal_zero tmp1;".into(),
                        "tmp1 <=X= val3;".into(),
                        // If the lower bits are zero, return the two's complement,
                        // otherwise return one's complement.
                        format!("val1 <== get_reg({});", rd.addr()),
                        format!("add_new_signed_2(- 1 + tmp1);"),
                    ],
                    rd,
                ))
                .collect()
        }
        "divu" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("divremu val1, val2;"),
                    rd,
                ))
                .collect()
        }
        "remu" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val4(
                    format!("divremu val1, val2;"),
                    rd,
                ))
                .collect()
        }

        // bitwise
        "xor" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(format!("xor 0;"), rd))
                .collect()
        }
        "xori" => {
            let (rd, r1, imm) = args.rri()?;
            read_args(vec![r1])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![format!("val2 <=X= 0;"), format!("xor {imm};")],
                    rd,
                ))
                .collect()
        }
        "and" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(format!("and 0;"), rd))
                .collect()
        }
        "andi" => {
            let (rd, r1, imm) = args.rri()?;
            read_args(vec![r1])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![format!("val2 <=X= 0;"), format!("and {imm};")],
                    rd,
                ))
                .collect()
        }
        "or" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(format!("or 0;"), rd))
                .collect()
        }
        "ori" => {
            let (rd, r1, imm) = args.rri()?;
            read_args(vec![r1])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![format!("val2 <=X= 0;"), format!("or {imm};")],
                    rd,
                ))
                .collect()
        }
        "not" => {
            let (rd, rs) = args.rr()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("add_new_signed_2 -1;"),
                    rd,
                ))
                .collect()
        }

        // shift
        "slli" => {
            let (rd, rs, amount) = args.rri()?;
            assert!(amount <= 31);
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    if amount <= 16 {
                        // rs is already in val1
                        vec![format!("val2 <=X= {};", 1 << amount), format!("wrap16;")]
                    } else {
                        vec![
                            // rs is already in val1
                            format!("val2 <=X= {};", 1 << 16),
                            format!("wrap16;"),
                            format!("tmp1 <=X= val3;"),
                            format!("val1 <=X= tmp1;"),
                            format!("val2 <=X= {};", 1 << (amount - 16)),
                            format!("wrap16;"),
                        ]
                    },
                    rd,
                ))
                .collect()
        }
        "sll" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        format!("val1 <== get_reg({});", r2.addr()),
                        format!("val2 <=X= 0;"),
                        format!("and 0x1f;"),
                        format!("tmp1 <=X= val3;"),
                        format!("val1 <== get_reg({});", r1.addr()),
                        format!("val2 <=X= tmp1;"),
                        format!("shl;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "srli" => {
            // logical shift right
            let (rd, rs, amount) = args.rri()?;
            assert!(amount <= 31);
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        // rs is already in val1
                        format!("val2 <=X= {amount};"),
                        format!("shr;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "srl" => {
            // logical shift right
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        format!("val1 <== get_reg({});", r2.addr()),
                        format!("val2 <=X= 0;"),
                        format!("and 0x1f;"),
                        format!("tmp1 <=X= val3;"),
                        format!("val1 <== get_reg({});", r1.addr()),
                        format!("val2 <=X= tmp1;"),
                        format!("shr;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "srai" => {
            // arithmetic shift right
            // TODO see if we can implement this directly with a machine.
            // Now we are using the equivalence
            // a >>> b = (a >= 0 ? a >> b : ~(~a >> b))
            let (rd, rs, amount) = args.rri()?;
            assert!(amount <= 31);
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        "to_signed;".into(),
                        "tmp1 <=X= val3;".into(),
                        format!("val1 <=X= tmp1;"),
                        format!("val2 <== get_reg(0);"),
                        format!("is_positive 0, -1;"),
                        format!("tmp1 <=X= val3;"),
                        format!("tmp1 <=X= tmp1 * 0xffffffff;"),
                        // Here, tmp1 is the full bit mask if rs is negative
                        // and zero otherwise.
                        format!("val1 <=X= tmp1;"),
                        format!("val2 <== get_reg({});", rs.addr()),
                        format!("xor 0;"),
                        format!("set_reg {}, val3;", rd.addr()),
                        format!("val1 <== get_reg({});", rd.addr()),
                        format!("val2 <=X= {amount};"),
                        format!("shr;"),
                        format!("set_reg {}, val3;", rd.addr()),
                        format!("val1 <=X= tmp1;"),
                        format!("val2 <== get_reg({});", rd.addr()),
                        format!("xor 0;"),
                    ],
                    rd,
                ))
                .collect()
        }

        // comparison
        "seqz" => {
            let (rd, rs) = args.rr()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("is_equal_zero val1;"),
                    rd,
                ))
                .collect()
        }
        "snez" => {
            let (rd, rs) = args.rr()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("is_not_equal_zero val1;"),
                    rd,
                ))
                .collect()
        }
        "slti" => {
            let (rd, rs, imm) = args.rri()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        "to_signed;".into(),
                        "tmp1 <=X= val3;".into(),
                        format!("val1 <=X= tmp1;"),
                        format!("val2 <== get_reg(0);"),
                        format!("is_positive {}, -1;", imm as i32),
                    ],
                    rd,
                ))
                .collect()
        }
        "slt" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        "to_signed;".into(),
                        "tmp1 <=X= val3;".into(),
                        format!("val1 <== get_reg({});", r2.addr()),
                        format!("to_signed;"),
                        "tmp2 <=X= val3;".into(),
                        format!("val1 <=X= tmp1;"),
                        format!("val2 <=X= tmp2;"),
                        format!("is_positive 0, -1;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "sltiu" => {
            let (rd, rs, imm) = args.rri()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        format!("val2 <== get_reg(0);"),
                        format!("is_positive {imm}, -1;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "sltu" => {
            let (rd, r1, r2) = args.rrr()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(
                    format!("is_positive 0, -1;"),
                    rd,
                ))
                .collect()
        }
        "sgtz" => {
            let (rd, rs) = args.rr()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        "to_signed;".into(),
                        "tmp1 <=X= val3;".into(),
                        format!("val1 <=X= tmp1;"),
                        format!("val2 <== get_reg(0);"),
                        format!("is_positive 0, 1;"),
                    ],
                    rd,
                ))
                .collect()
        }

        // branching
        "beq" => {
            let (r1, r2, label) = args.rrl()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(vec![format!("branch_if_zero {r1} - {r2}, {label};")])
                .collect()
        }
        "beqz" => {
            let (r1, label) = args.rl()?;
            read_args(vec![r1])
                .into_iter()
                .chain(vec![format!("branch_if_zero {r1}, {label};")])
                .collect()
        }
        "bgeu" => {
            let (r1, r2, label) = args.rrl()?;
            // TODO does this fulfill the input requirements for branch_if_positive?
            read_args(vec![r1, r2])
                .into_iter()
                .chain(vec![format!(
                    "branch_if_positive {r1} - {r2} + 1, {label};"
                )])
                .collect()
        }
        "bgez" => {
            let (r1, label) = args.rl()?;
            read_args(vec![r1])
                .into_iter()
                .chain(vec![
                    "to_signed;".into(),
                    "tmp1 <=X= val3;".into(),
                    format!("branch_if_positive tmp1 + 1, {label};"),
                ])
                .collect()
        }
        "bltu" => {
            let (r1, r2, label) = args.rrl()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(vec![format!("branch_if_positive {r2} - {r1}, {label};")])
                .collect()
        }
        "blt" => {
            let (r1, r2, label) = args.rrl()?;
            // Branch if r1 < r2 (signed).
            // TODO does this fulfill the input requirements for branch_if_positive?
            read_args(vec![r1, r2])
                .into_iter()
                .chain(vec![
                    "to_signed;".into(),
                    "tmp1 <=X= val3;".into(),
                    format!("val1 <== get_reg({});", r2.addr()),
                    "to_signed;".into(),
                    "tmp2 <=X= val3;".into(),
                    format!("branch_if_positive tmp2 - tmp1, {label};"),
                ])
                .collect()
        }
        "bge" => {
            let (r1, r2, label) = args.rrl()?;
            // Branch if r1 >= r2 (signed).
            // TODO does this fulfill the input requirements for branch_if_positive?
            read_args(vec![r1, r2])
                .into_iter()
                .chain(vec![
                    "to_signed;".into(),
                    "tmp1 <=X= val3;".into(),
                    format!("val1 <== get_reg({});", r2.addr()),
                    "to_signed;".into(),
                    "tmp2 <=X= val3;".into(),
                    format!("branch_if_positive tmp1 - tmp2 + 1, {label};"),
                ])
                .collect()
        }
        "bltz" => {
            // branch if 2**31 <= r1 < 2**32
            let (r1, label) = args.rl()?;
            read_args(vec![r1])
                .into_iter()
                .chain(vec![format!(
                    "branch_if_positive {r1} - 2**31 + 1, {label};"
                )])
                .collect()
        }
        "blez" => {
            // branch less or equal zero
            let (r1, label) = args.rl()?;
            read_args(vec![r1])
                .into_iter()
                .chain(vec![
                    "to_signed;".into(),
                    "tmp1 <=X= val3;".into(),
                    format!("branch_if_positive -tmp1 + 1, {label};"),
                ])
                .collect()
        }
        "bgtz" => {
            // branch if 0 < r1 < 2**31
            let (r1, label) = args.rl()?;
            read_args(vec![r1])
                .into_iter()
                .chain(vec![
                    "to_signed;".into(),
                    "tmp1 <=X= val3;".into(),
                    format!("branch_if_positive tmp1, {label};"),
                ])
                .collect()
        }
        "bne" => {
            let (r1, r2, label) = args.rrl()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(vec![format!("branch_if_nonzero {label};")])
                .collect()
        }
        "bnez" => {
            let (r1, label) = args.rl()?;
            // jump if (r1 - x0) = r1 != 0
            read_args(vec![r1, Register::new(0)])
                .into_iter()
                .chain(vec![format!("branch_if_nonzero {label};")])
                .collect()
        }

        // jump and call
        "j" | "tail" => {
            let label = args.l()?;
            vec![format!("jump {label};",)]
        }
        "jr" => {
            let rs = args.r()?;
            read_args(vec![rs])
                .into_iter()
                .chain(vec![
                    format!("val1 <== get_reg({});", rs.addr()),
                    format!("jump_dyn;"),
                ])
                .collect()
        }
        "jal" => {
            if let Ok(label) = args.l() {
                vec!["set_reg 1, pc + 2;".to_string(), format!("jump {label};")]
            } else {
                let (rd, label) = args.rl()?;
                if rd.is_zero() {
                    vec![format!("jump {label};")]
                } else {
                    vec![
                        format!("set_reg {}, pc + 2;", rd.addr()),
                        format!("jump {label};"),
                    ]
                }
            }
        }
        "jalr" => {
            if let Ok(rs) = args.r() {
                read_args(vec![rs])
                    .into_iter()
                    .chain([
                        format!("val1 <== get_reg({});", rs.addr()),
                        "set_reg 1, pc + 2;".to_string(),
                        format!("jump_dyn;"),
                    ])
                    .collect()
            } else {
                let (rd, rs, off) = args.rro()?;
                assert_eq!(off, 0, "jalr with non-zero offset is not supported");
                if rd.is_zero() {
                    read_args(vec![rs])
                        .into_iter()
                        .chain([
                            format!("val1 <== get_reg({});", rs.addr()),
                            format!("jump_dyn;"),
                        ])
                        .collect()
                } else {
                    read_args(vec![rs])
                        .into_iter()
                        .chain([format!("val1 <== get_reg({});", rs.addr())])
                        .chain([format!("set_reg {}, pc + 2;", rd.addr())])
                        .chain([format!("jump_dyn;")])
                        .collect()
                }
            }
        }
        "call" => {
            let label = args.l()?;
            vec![format!("set_reg 1, pc + 2;"), format!("jump {label};")]
        }
        "ecall" => {
            args.empty()?;
            // save ra/x1
            push_register("x1")
                .into_iter()
                // jump to to handler
                .chain([
                    "set_reg 1, pc + 2;".to_string(),
                    "jump __ecall_handler;".to_string(),
                ])
                // restore ra/x1
                .chain(pop_register("x1"))
                .collect()
        }
        "ebreak" => {
            args.empty()?;
            // we don't use ebreak for anything, ignore
            vec![]
        }
        "ret" => {
            args.empty()?;
            vec![
                format!("val1 <== get_reg(1);"),
                "val1 <== get_reg(1);".to_string(),
                "jump_dyn;".to_string(),
            ]
        }

        // memory access
        "lw" => {
            let (rd, rs, off) = args.rro()?;
            // TODO we need to consider misaligned loads / stores
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_val3(format!("mload {off};"), rd))
                .collect()
        }
        "lb" => {
            // load byte and sign-extend. the memory is little-endian.
            let (rd, rs, off) = args.rro()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        format!("mload {off};"),
                        format!("val1 <=X= val3;"),
                        format!("tmp2 <=X= val4;"),
                        format!("val2 <=X= 8 * tmp2;"),
                        format!("shr;"),
                        format!("val1 <=X= val3;"),
                        format!("sign_extend_byte;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "lbu" => {
            // load byte and zero-extend. the memory is little-endian.
            let (rd, rs, off) = args.rro()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        format!("mload {off};"),
                        format!("val1 <=X= val3;"),
                        format!("tmp2 <=X= val4;"),
                        format!("val2 <=X= 8 * tmp2;"),
                        format!("shr;"),
                        format!("{rd} <=X= val3;"),
                        format!("set_reg {}, {rd};", rd.addr()),
                        format!("val1 <== get_reg({});", rd.addr()),
                        format!("val2 <=X= 0;"),
                        format!("and 0xff;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "lh" => {
            // Load two bytes and sign-extend.
            // Assumes the address is a multiple of two.
            let (rd, rs, off) = args.rro()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        format!("mload {off};"),
                        format!("val1 <=X= val3;"),
                        format!("tmp2 <=X= val4;"),
                        format!("val2 <=X= 8 * tmp2;"),
                        format!("shr;"),
                        format!("val1 <=X= val3;"),
                        format!("sign_extend_16_bits;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "lhu" => {
            // Load two bytes and zero-extend.
            // Assumes the address is a multiple of two.
            let (rd, rs, off) = args.rro()?;
            read_args(vec![rs])
                .into_iter()
                .chain(only_if_no_write_to_zero_vec_val3(
                    vec![
                        format!("mload {off};"),
                        format!("val1 <=X= val3;"),
                        format!("tmp2 <=X= val4;"),
                        format!("val2 <=X= 8 * tmp2;"),
                        format!("shr;"),
                        format!("{rd} <=X= val3;"),
                        format!("set_reg {}, {rd};", rd.addr()),
                        format!("val1 <== get_reg({});", rd.addr()),
                        format!("val2 <=X= 0;"),
                        format!("and 0x0000ffff;"),
                    ],
                    rd,
                ))
                .collect()
        }
        "sw" => {
            let (r1, r2, off) = args.rro()?;
            read_args(vec![r1, r2])
                .into_iter()
                .chain(vec![
                    format!("val2 <== get_reg({});", r1.addr()),
                    format!("val1 <== get_reg({});", r2.addr()),
                    format!("mstore {off};"),
                ])
                .collect()
        }
        "sh" => {
            // store half word (two bytes)
            // TODO this code assumes it is at least aligned on
            // a two-byte boundary

            let (rs, rd, off) = args.rro()?;
            read_args(vec![rs, rd])
                .into_iter()
                .chain(vec![
                    format!("val1 <=X= val2;"),
                    format!("mload {off};"),
                    format!("tmp1 <=X= val3;"),
                    format!("tmp2 <=X= val4;"),
                    "val1 <=X= 0xffff;".to_string(),
                    "val2 <=X= 8 * tmp2;".to_string(),
                    "shl;".to_string(),
                    "tmp3 <=X= val3;".to_string(),
                    "val1 <=X= tmp3;".to_string(),
                    "val2 <=X= 0;".to_string(),
                    "xor 0xffffffff;".to_string(),
                    "tmp3 <=X= val3;".to_string(),
                    "val1 <=X= tmp1;".to_string(),
                    "val2 <=X= tmp3;".to_string(),
                    "and 0;".to_string(),
                    "tmp1 <=X= val3;".to_string(),
                    format!("val1 <== get_reg({});", rs.addr()),
                    "val2 <=X= 0;".to_string(),
                    "and 0xffff;".to_string(),
                    "tmp3 <=X= val3;".to_string(),
                    "val1 <=X= tmp3;".to_string(),
                    "val2 <=X= 8 * tmp2;".to_string(),
                    "shl;".to_string(),
                    "tmp3 <=X= val3;".to_string(),
                    "val1 <=X= tmp1;".to_string(),
                    "val2 <=X= tmp3;".to_string(),
                    "or 0;".to_string(),
                    "tmp1 <=X= val3;".to_string(),
                    format!("val2 <=X= tmp1;"),
                    format!("val1 <== get_reg({});", rd.addr()),
                    format!("mstore {off} - tmp2;"),
                ])
                .collect()
        }
        "sb" => {
            // store byte
            let (rs, rd, off) = args.rro()?;
            read_args(vec![rs, rd])
                .into_iter()
                .chain(vec![
                    format!("val1 <=X= val2;"),
                    format!("mload {off};"),
                    format!("tmp1 <=X= val3;"),
                    format!("tmp2 <=X= val4;"),
                    "val1 <=X= 0xff;".to_string(),
                    "val2 <=X= 8 * tmp2;".to_string(),
                    "shl;".to_string(),
                    "tmp3 <=X= val3;".to_string(),
                    "val1 <=X= tmp3;".to_string(),
                    "val2 <=X= 0;".to_string(),
                    "xor 0xffffffff;".to_string(),
                    "tmp3 <=X= val3;".to_string(),
                    "val1 <=X= tmp1;".to_string(),
                    "val2 <=X= tmp3;".to_string(),
                    "and 0;".to_string(),
                    "tmp1 <=X= val3;".to_string(),
                    format!("val1 <== get_reg({});", rs.addr()),
                    "val2 <=X= 0;".to_string(),
                    format!("and 0xff;"),
                    "tmp3 <=X= val3;".to_string(),
                    "val1 <=X= tmp3;".to_string(),
                    "val2 <=X= 8 * tmp2;".to_string(),
                    "shl;".to_string(),
                    "tmp3 <=X= val3;".to_string(),
                    "val1 <=X= tmp1;".to_string(),
                    "val2 <=X= tmp3;".to_string(),
                    "or 0;".to_string(),
                    "tmp1 <=X= val3;".to_string(),
                    format!("val2 <=X= tmp1;"),
                    format!("val1 <== get_reg({});", rd.addr()),
                    format!("mstore {off} - tmp2;"),
                ])
                .collect()
        }
        "fence" | "fence.i" | "nop" => vec![],
        "unimp" => vec!["fail;".to_string()],

        // atomic instructions
        insn if insn.starts_with("amoadd.w") => {
            let (rd, rs2, rs1, off) = args.rrro()?;
            assert_eq!(off, 0);

            [
                read_args(vec![rs1, rs2]),
                // val1 = rs1, val2 = rs2
                vec![
                    format!("mload 0;"),
                    format!("tmp1 <=X= val3;"),
                    format!("tmp2 <=X= val4;"),
                    format!("val1 <=X= tmp1;"),
                    format!("add_new;"),
                    format!("tmp2 <=X= val3;"),
                    format!("val2 <=X= tmp2;"),
                    format!("val1 <== get_reg({});", rs1.addr()),
                    format!("mstore 0;"),
                ],
                only_if_no_write_to_zero_val3(format!("val3 <=X= tmp1;"), rd),
            ]
            .concat()
        }

        insn if insn.starts_with("lr.w") => {
            // Very similar to "lw":
            let (rd, rs, off) = args.rro()?;
            assert_eq!(off, 0);
            // TODO misaligned access should raise misaligned address exceptions
            [
                read_args(vec![rs]),
                only_if_no_write_to_zero_vec_val3(
                    vec![format!("mload 0;"), format!("tmp1 <=X= val4;")],
                    rd,
                ),
                vec!["lr_sc_reservation <=X= 1;".into()],
            ]
            .concat()
        }

        insn if insn.starts_with("sc.w") => {
            // Some overlap with "sw", but also writes 0 to rd on success
            let (rd, rs2, rs1, off) = args.rrro()?;
            assert_eq!(off, 0);
            // TODO: misaligned access should raise misaligned address exceptions
            [
                "skip_if_zero lr_sc_reservation, 3;".into(),
                format!("val1 <== get_reg({});", rs1.addr()),
                format!("val2 <== get_reg({});", rs2.addr()),
                format!("mstore 0;"),
            ]
            .into_iter()
            .chain(only_if_no_write_to_zero_val3(
                format!("val3 <=X= (1 - lr_sc_reservation);"),
                rd,
            ))
            .chain(["lr_sc_reservation <=X= 0;".into()])
            .collect()
        }

        _ => {
            panic!("Unknown instruction: {instr}");
        }
    };
    for s in &statements {
        log::debug!("          {s}");
    }
    Ok(statements)
}
