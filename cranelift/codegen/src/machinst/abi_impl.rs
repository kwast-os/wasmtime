//! Implementation of a vanilla ABI, shared between several machines. The
//! implementation here assumes that arguments will be passed in registers
//! first, then additional args on the stack; that the stack grows downward,
//! contains a standard frame (return address and frame pointer), and the
//! compiler is otherwise free to allocate space below that with its choice of
//! layout; and that the machine has some notion of caller- and callee-save
//! registers. Most modern machines, e.g. x86-64 and AArch64, should fit this
//! mold and thus both of these backends use this shared implementation.
//!
//! See the documentation in specific machine backends for the "instantiation"
//! of this generic ABI, i.e., which registers are caller/callee-save, arguments
//! and return values, and any other special requirements.
//!
//! For now the implementation here assumes a 64-bit machine, but we intend to
//! make this 32/64-bit-generic shortly.
//!
//! # Vanilla ABI
//!
//! First, arguments and return values are passed in registers up to a certain
//! fixed count, after which they overflow onto the stack. Multiple return
//! values either fit in registers, or are returned in a separate return-value
//! area on the stack, given by a hidden extra parameter.
//!
//! Note that the exact stack layout is up to us. We settled on the
//! below design based on several requirements. In particular, we need to be
//! able to generate instructions (or instruction sequences) to access
//! arguments, stack slots, and spill slots before we know how many spill slots
//! or clobber-saves there will be, because of our pass structure. We also
//! prefer positive offsets to negative offsets because of an asymmetry in
//! some machines' addressing modes (e.g., on AArch64, positive offsets have a
//! larger possible range without a long-form sequence to synthesize an
//! arbitrary offset). Finally, it is not allowed to access memory below the
//! current SP value.
//!
//! We assume that a prologue first pushes the frame pointer (and return address
//! above that, if the machine does not do that in hardware). We set FP to point
//! to this two-word frame record. We store all other frame slots below this
//! two-word frame record, with the stack pointer remaining at or below this
//! fixed frame storage for the rest of the function. We can then access frame
//! storage slots using positive offsets from SP. In order to allow codegen for
//! the latter before knowing how many clobber-saves we have, and also allow it
//! while SP is being adjusted to set up a call, we implement a "nominal SP"
//! tracking feature by which a fixup (distance between actual SP and a
//! "nominal" SP) is known at each instruction.
//!
//! # Stack Layout
//!
//! The stack looks like:
//!
//! ```plain
//!   (high address)
//!
//!                              +---------------------------+
//!                              |          ...              |
//!                              | stack args                |
//!                              | (accessed via FP)         |
//!                              +---------------------------+
//! SP at function entry ----->  | return address            |
//!                              +---------------------------+
//! FP after prologue -------->  | FP (pushed by prologue)   |
//!                              +---------------------------+
//!                              |          ...              |
//!                              | spill slots               |
//!                              | (accessed via nominal SP) |
//!                              |          ...              |
//!                              | stack slots               |
//!                              | (accessed via nominal SP) |
//! nominal SP --------------->  | (alloc'd by prologue)     |
//!                              +---------------------------+
//!                              |          ...              |
//!                              | clobbered callee-saves    |
//! SP at end of prologue ---->  | (pushed by prologue)      |
//!                              +---------------------------+
//!                              | [alignment as needed]     |
//!                              |          ...              |
//!                              | args for call             |
//! SP before making a call -->  | (pushed at callsite)      |
//!                              +---------------------------+
//!
//!   (low address)
//! ```
//!
//! # Multi-value Returns
//!
//! Note that we support multi-value returns in two ways. First, we allow for
//! multiple return-value registers. Second, if teh appropriate flag is set, we
//! support the SpiderMonkey Wasm ABI.  For details of the multi-value return
//! ABI, see:
//!
//! https://searchfox.org/mozilla-central/rev/bc3600def806859c31b2c7ac06e3d69271052a89/js/src/wasm/WasmStubs.h#134
//!
//! In brief:
//! - Return values are processed in *reverse* order.
//! - The first return value in this order (so the last return) goes into the
//!   ordinary return register.
//! - Any further returns go in a struct-return area, allocated upwards (in
//!   address order) during the reverse traversal.
//! - This struct-return area is provided by the caller, and a pointer to its
//!   start is passed as an invisible last (extra) argument. Normally the caller
//!   will allocate this area on the stack. When we generate calls, we place it
//!   just above the on-stack argument area.
//! - So, for example, a function returning 4 i64's (v0, v1, v2, v3), with no
//!   formal arguments, would:
//!   - Accept a pointer `P` to the struct return area as a hidden argument in the
//!     first argument register on entry.
//!   - Return v3 in the one and only return-value register.
//!   - Return v2 in memory at `[P]`.
//!   - Return v1 in memory at `[P+8]`.
//!   - Return v0 in memory at `[P+16]`.

use super::abi::*;
use crate::binemit::StackMap;
use crate::ir::types::*;
use crate::ir::{ArgumentExtension, SourceLoc, StackSlot};
use crate::machinst::*;
use crate::settings;
use crate::CodegenResult;
use crate::{ir, isa};
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::mem;
use log::{debug, trace};
use regalloc::{RealReg, Reg, RegClass, Set, SpillSlot, Writable};

/// A location for an argument or return value.
#[derive(Clone, Copy, Debug)]
pub enum ABIArg {
    /// In a real register.
    Reg(RealReg, ir::Type, ir::ArgumentExtension),
    /// Arguments only: on stack, at given offset from SP at entry.
    Stack(i64, ir::Type, ir::ArgumentExtension),
}

/// Are we computing information about arguments or return values? Much of the
/// handling is factored out into common routines; this enum allows us to
/// distinguish which case we're handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgsOrRets {
    /// Arguments.
    Args,
    /// Return values.
    Rets,
}

/// Abstract location for a machine-specific ABI impl to translate into the
/// appropriate addressing mode.
#[derive(Clone, Copy, Debug)]
pub enum StackAMode {
    /// Offset from the frame pointer, possibly making use of a specific type
    /// for a scaled indexing operation.
    FPOffset(i64, ir::Type),
    /// Offset from the nominal stack pointer, possibly making use of a specific
    /// type for a scaled indexing operation.
    NominalSPOffset(i64, ir::Type),
    /// Offset from the real stack pointer, possibly making use of a specific
    /// type for a scaled indexing operation.
    SPOffset(i64, ir::Type),
}

/// Trait implemented by machine-specific backend to provide information about
/// register assignments and to allow generating the specific instructions for
/// stack loads/saves, prologues/epilogues, etc.
pub trait ABIMachineImpl {
    /// The instruction type.
    type I: VCodeInst;

    /// Process a list of parameters or return values and allocate them to registers
    /// and stack slots.
    ///
    /// Returns the list of argument locations, the stack-space used (rounded up
    /// to as alignment requires), and if `add_ret_area_ptr` was passed, the
    /// index of the extra synthetic arg that was added.
    fn compute_arg_locs(
        call_conv: isa::CallConv,
        params: &[ir::AbiParam],
        args_or_rets: ArgsOrRets,
        add_ret_area_ptr: bool,
    ) -> CodegenResult<(Vec<ABIArg>, i64, Option<usize>)>;

    /// Returns the offset from FP to the argument area, i.e., jumping over the saved FP, return
    /// address, and maybe other standard elements depending on ABI (e.g. Wasm TLS reg).
    fn fp_to_arg_offset(call_conv: isa::CallConv, flags: &settings::Flags) -> i64;

    /// Generate a load from the stack.
    fn gen_load_stack(mem: StackAMode, into_reg: Writable<Reg>, ty: Type) -> Self::I;

    /// Generate a store to the stack.
    fn gen_store_stack(mem: StackAMode, from_reg: Reg, ty: Type) -> Self::I;

    /// Generate a move.
    fn gen_move(to_reg: Writable<Reg>, from_reg: Reg, ty: Type) -> Self::I;

    /// Generate an integer-extend operation.
    fn gen_extend(
        to_reg: Writable<Reg>,
        from_reg: Reg,
        is_signed: bool,
        from_bits: u8,
        to_bits: u8,
    ) -> Self::I;

    /// Generate a return instruction.
    fn gen_ret() -> Self::I;

    /// Generate an "epilogue placeholder" instruction, recognized by lowering
    /// when using the Baldrdash ABI.
    fn gen_epilogue_placeholder() -> Self::I;

    /// Generate an add-with-immediate. Note that even if this uses a scratch
    /// register, the sequence must still be correct if the given source or dest
    /// is the register returned by `get_fixed_tmp_reg()`; hence, for machines
    /// that may need a scratch register to synthesize an arbitrary constant,
    /// the machine backend should reserve *another* fixed temp register for
    /// this purpose. (E.g., on AArch64, x16 is the ordinary fixed tmp, and x17
    /// is the secondary fixed tmp used to implement this.)
    fn gen_add_imm(into_reg: Writable<Reg>, from_reg: Reg, imm: u64) -> SmallVec<[Self::I; 4]>;

    /// Generate a sequence that traps with a `TrapCode::StackOverflow` code if
    /// the stack pointer is less than the given limit register (assuming the
    /// stack grows downward).
    fn gen_stack_lower_bound_trap(limit_reg: Reg) -> SmallVec<[Self::I; 2]>;

    /// Generate an instruction to compute an address of a stack slot (FP- or
    /// SP-based offset).
    fn gen_get_stack_addr(mem: StackAMode, into_reg: Writable<Reg>, ty: Type) -> Self::I;

    /// Get a fixed (not used by regalloc) temp. This is needed for certain
    /// sequences generated after the register allocator has already run.
    fn get_fixed_tmp_reg() -> Reg;

    /// Generate a store to the given [base+offset] address.
    fn gen_load_base_offset(into_reg: Writable<Reg>, base: Reg, offset: i64, ty: Type) -> Self::I;

    /// Generate a load from the given [base+offset] address.
    fn gen_store_base_offset(base: Reg, offset: i64, from_reg: Reg, ty: Type) -> Self::I;

    /// Adjust the stack pointer up or down.
    fn gen_sp_reg_adjust(amount: i64) -> SmallVec<[Self::I; 2]>;

    /// Generate a meta-instruction that adjusts the nominal SP offset.
    fn gen_nominal_sp_adj(amount: i64) -> Self::I;

    /// Generate the usual frame-setup sequence for this architecture: e.g.,
    /// `push rbp / mov rbp, rsp` on x86-64, or `stp fp, lr, [sp, #-16]!` on
    /// AArch64.
    fn gen_prologue_frame_setup() -> SmallVec<[Self::I; 2]>;

    /// Generate the usual frame-restore sequence for this architecture.
    fn gen_epilogue_frame_restore() -> SmallVec<[Self::I; 2]>;

    /// Generate a clobber-save sequence. This takes the list of *all* registers
    /// written/modified by the function body. The implementation here is
    /// responsible for determining which of these are callee-saved according to
    /// the ABI. It should return a sequence of instructions that "push" or
    /// otherwise save these values to the stack. The sequence of instructions
    /// should adjust the stack pointer downward, and should align as necessary
    /// according to ABI requirements.
    ///
    /// Returns stack bytes used as well as instructions. Does not adjust
    /// nominal SP offset; caller will do that.
    fn gen_clobber_save(
        call_conv: isa::CallConv,
        clobbers: &Set<Writable<RealReg>>,
    ) -> (u64, SmallVec<[Self::I; 16]>);

    /// Generate a clobber-restore sequence. This sequence should perform the
    /// opposite of the clobber-save sequence generated above, assuming that SP
    /// going into the sequence is at the same point that it was left when the
    /// clobber-save sequence finished.
    fn gen_clobber_restore(
        call_conv: isa::CallConv,
        clobbers: &Set<Writable<RealReg>>,
    ) -> SmallVec<[Self::I; 16]>;

    /// Generate a call instruction/sequence.
    fn gen_call(
        dest: &CallDest,
        uses: Vec<Reg>,
        defs: Vec<Writable<Reg>>,
        loc: SourceLoc,
        opcode: ir::Opcode,
    ) -> SmallVec<[(/* is_safepoint = */ bool, Self::I); 2]>;

    /// Get the number of spillslots required for the given register-class and
    /// type.
    fn get_spillslot_size(rc: RegClass, ty: Type) -> u32;

    /// Get the current virtual-SP offset from an instruction-emission state.
    fn get_virtual_sp_offset_from_state(s: &<Self::I as MachInstEmit>::State) -> i64;

    /// Get the "nominal SP to FP" offset from an instruction-emission state.
    fn get_nominal_sp_to_fp(s: &<Self::I as MachInstEmit>::State) -> i64;

    /// Get all caller-save registers.
    fn get_caller_saves(call_conv: isa::CallConv) -> Vec<Writable<Reg>>;
}

/// ABI information shared between body (callee) and caller.
struct ABISig {
    /// Argument locations (regs or stack slots). Stack offsets are relative to
    /// SP on entry to function.
    args: Vec<ABIArg>,
    /// Return-value locations. Stack offsets are relative to the return-area
    /// pointer.
    rets: Vec<ABIArg>,
    /// Space on stack used to store arguments.
    stack_arg_space: i64,
    /// Space on stack used to store return values.
    stack_ret_space: i64,
    /// Index in `args` of the stack-return-value-area argument.
    stack_ret_arg: Option<usize>,
    /// Calling convention used.
    call_conv: isa::CallConv,
}

impl ABISig {
    fn from_func_sig<M: ABIMachineImpl>(sig: &ir::Signature) -> CodegenResult<ABISig> {
        // Compute args and retvals from signature. Handle retvals first,
        // because we may need to add a return-area arg to the args.
        let (rets, stack_ret_space, _) = M::compute_arg_locs(
            sig.call_conv,
            &sig.returns,
            ArgsOrRets::Rets,
            /* extra ret-area ptr = */ false,
        )?;
        let need_stack_return_area = stack_ret_space > 0;
        let (args, stack_arg_space, stack_ret_arg) = M::compute_arg_locs(
            sig.call_conv,
            &sig.params,
            ArgsOrRets::Args,
            need_stack_return_area,
        )?;

        trace!(
            "ABISig: sig {:?} => args = {:?} rets = {:?} arg stack = {} ret stack = {} stack_ret_arg = {:?}",
            sig,
            args,
            rets,
            stack_arg_space,
            stack_ret_space,
            stack_ret_arg
        );

        Ok(ABISig {
            args,
            rets,
            stack_arg_space,
            stack_ret_space,
            stack_ret_arg,
            call_conv: sig.call_conv,
        })
    }
}

/// ABI object for a function body.
pub struct ABIBodyImpl<M: ABIMachineImpl> {
    /// Signature: arg and retval regs.
    sig: ABISig,
    /// Offsets to each stackslot.
    stackslots: Vec<u32>,
    /// Total stack size of all stackslots.
    stackslots_size: u32,
    /// Clobbered registers, from regalloc.
    clobbered: Set<Writable<RealReg>>,
    /// Total number of spillslots, from regalloc.
    spillslots: Option<usize>,
    /// "Total frame size", as defined by "distance between FP and nominal SP".
    /// Some items are pushed below nominal SP, so the function may actually use
    /// more stack than this would otherwise imply. It is simply the initial
    /// frame/allocation size needed for stackslots and spillslots.
    total_frame_size: Option<u32>,
    /// The register holding the return-area pointer, if needed.
    ret_area_ptr: Option<Writable<Reg>>,
    /// Calling convention this function expects.
    call_conv: isa::CallConv,
    /// The settings controlling this function's compilation.
    flags: settings::Flags,
    /// Whether or not this function is a "leaf", meaning it calls no other
    /// functions
    is_leaf: bool,
    /// If this function has a stack limit specified, then `Reg` is where the
    /// stack limit will be located after the instructions specified have been
    /// executed.
    ///
    /// Note that this is intended for insertion into the prologue, if
    /// present. Also note that because the instructions here execute in the
    /// prologue this happens after legalization/register allocation/etc so we
    /// need to be extremely careful with each instruction. The instructions are
    /// manually register-allocated and carefully only use caller-saved
    /// registers and keep nothing live after this sequence of instructions.
    stack_limit: Option<(Reg, Vec<M::I>)>,

    _mach: PhantomData<M>,
}

fn get_special_purpose_param_register(
    f: &ir::Function,
    abi: &ABISig,
    purpose: ir::ArgumentPurpose,
) -> Option<Reg> {
    let idx = f.signature.special_param_index(purpose)?;
    match abi.args[idx] {
        ABIArg::Reg(reg, ..) => Some(reg.to_reg()),
        ABIArg::Stack(..) => None,
    }
}

impl<M: ABIMachineImpl> ABIBodyImpl<M> {
    /// Create a new body ABI instance.
    pub fn new(f: &ir::Function, flags: settings::Flags) -> CodegenResult<Self> {
        debug!("ABI: func signature {:?}", f.signature);

        let sig = ABISig::from_func_sig::<M>(&f.signature)?;

        let call_conv = f.signature.call_conv;
        // Only these calling conventions are supported.
        debug_assert!(
            call_conv == isa::CallConv::SystemV
                || call_conv == isa::CallConv::Fast
                || call_conv == isa::CallConv::Cold
                || call_conv.extends_baldrdash(),
            "Unsupported calling convention: {:?}",
            call_conv
        );

        // Compute stackslot locations and total stackslot size.
        let mut stack_offset: u32 = 0;
        let mut stackslots = vec![];
        for (stackslot, data) in f.stack_slots.iter() {
            let off = stack_offset;
            stack_offset += data.size;
            stack_offset = (stack_offset + 7) & !7;
            debug_assert_eq!(stackslot.as_u32() as usize, stackslots.len());
            stackslots.push(off);
        }

        // Figure out what instructions, if any, will be needed to check the
        // stack limit. This can either be specified as a special-purpose
        // argument or as a global value which often calculates the stack limit
        // from the arguments.
        let stack_limit =
            get_special_purpose_param_register(f, &sig, ir::ArgumentPurpose::StackLimit)
                .map(|reg| (reg, Vec::new()))
                .or_else(|| f.stack_limit.map(|gv| gen_stack_limit::<M>(f, &sig, gv)));

        Ok(Self {
            sig,
            stackslots,
            stackslots_size: stack_offset,
            clobbered: Set::empty(),
            spillslots: None,
            total_frame_size: None,
            ret_area_ptr: None,
            call_conv,
            flags,
            is_leaf: f.is_leaf(),
            stack_limit,
            _mach: PhantomData,
        })
    }

    /// Inserts instructions necessary for checking the stack limit into the
    /// prologue.
    ///
    /// This function will generate instructions necessary for perform a stack
    /// check at the header of a function. The stack check is intended to trap
    /// if the stack pointer goes below a particular threshold, preventing stack
    /// overflow in wasm or other code. The `stack_limit` argument here is the
    /// register which holds the threshold below which we're supposed to trap.
    /// This function is known to allocate `stack_size` bytes and we'll push
    /// instructions onto `insts`.
    ///
    /// Note that the instructions generated here are special because this is
    /// happening so late in the pipeline (e.g. after register allocation). This
    /// means that we need to do manual register allocation here and also be
    /// careful to not clobber any callee-saved or argument registers. For now
    /// this routine makes do with the `spilltmp_reg` as one temporary
    /// register, and a second register of `tmp2` which is caller-saved. This
    /// should be fine for us since no spills should happen in this sequence of
    /// instructions, so our register won't get accidentally clobbered.
    ///
    /// No values can be live after the prologue, but in this case that's ok
    /// because we just need to perform a stack check before progressing with
    /// the rest of the function.
    fn insert_stack_check(&self, stack_limit: Reg, stack_size: u32, insts: &mut Vec<M::I>) {
        // With no explicit stack allocated we can just emit the simple check of
        // the stack registers against the stack limit register, and trap if
        // it's out of bounds.
        if stack_size == 0 {
            insts.extend(M::gen_stack_lower_bound_trap(stack_limit));
            return;
        }

        // Note that the 32k stack size here is pretty special. See the
        // documentation in x86/abi.rs for why this is here. The general idea is
        // that we're protecting against overflow in the addition that happens
        // below.
        if stack_size >= 32 * 1024 {
            insts.extend(M::gen_stack_lower_bound_trap(stack_limit));
        }

        // Add the `stack_size` to `stack_limit`, placing the result in
        // `scratch`.
        //
        // Note though that `stack_limit`'s register may be the same as
        // `scratch`. If our stack size doesn't fit into an immediate this
        // means we need a second scratch register for loading the stack size
        // into a register.
        let scratch = Writable::from_reg(M::get_fixed_tmp_reg());
        let stack_size = u64::from(stack_size);
        insts.extend(M::gen_add_imm(scratch, stack_limit, stack_size).into_iter());
        insts.extend(M::gen_stack_lower_bound_trap(scratch.to_reg()));
    }
}

/// Generates the instructions necessary for the `gv` to be materialized into a
/// register.
///
/// This function will return a register that will contain the result of
/// evaluating `gv`. It will also return any instructions necessary to calculate
/// the value of the register.
///
/// Note that global values are typically lowered to instructions via the
/// standard legalization pass. Unfortunately though prologue generation happens
/// so late in the pipeline that we can't use these legalization passes to
/// generate the instructions for `gv`. As a result we duplicate some lowering
/// of `gv` here and support only some global values. This is similar to what
/// the x86 backend does for now, and hopefully this can be somewhat cleaned up
/// in the future too!
///
/// Also note that this function will make use of `writable_spilltmp_reg()` as a
/// temporary register to store values in if necessary. Currently after we write
/// to this register there's guaranteed to be no spilled values between where
/// it's used, because we're not participating in register allocation anyway!
fn gen_stack_limit<M: ABIMachineImpl>(
    f: &ir::Function,
    abi: &ABISig,
    gv: ir::GlobalValue,
) -> (Reg, Vec<M::I>) {
    let mut insts = Vec::new();
    let reg = generate_gv::<M>(f, abi, gv, &mut insts);
    return (reg, insts);
}

fn generate_gv<M: ABIMachineImpl>(
    f: &ir::Function,
    abi: &ABISig,
    gv: ir::GlobalValue,
    insts: &mut Vec<M::I>,
) -> Reg {
    match f.global_values[gv] {
        // Return the direct register the vmcontext is in
        ir::GlobalValueData::VMContext => {
            get_special_purpose_param_register(f, abi, ir::ArgumentPurpose::VMContext)
                .expect("no vmcontext parameter found")
        }
        // Load our base value into a register, then load from that register
        // in to a temporary register.
        ir::GlobalValueData::Load {
            base,
            offset,
            global_type: _,
            readonly: _,
        } => {
            let base = generate_gv::<M>(f, abi, base, insts);
            let into_reg = Writable::from_reg(M::get_fixed_tmp_reg());
            insts.push(M::gen_load_base_offset(into_reg, base, offset.into(), I64));
            return into_reg.to_reg();
        }
        ref other => panic!("global value for stack limit not supported: {}", other),
    }
}

/// Return a type either from an optional type hint, or if not, from the default
/// type associated with the given register's class. This is used to generate
/// loads/spills appropriately given the type of value loaded/stored (which may
/// be narrower than the spillslot). We usually have the type because the
/// regalloc usually provides the vreg being spilled/reloaded, and we know every
/// vreg's type. However, the regalloc *can* request a spill/reload without an
/// associated vreg when needed to satisfy a safepoint (which requires all
/// ref-typed values, even those in real registers in the original vcode, to be
/// in spillslots).
fn ty_from_ty_hint_or_reg_class(r: Reg, ty: Option<Type>) -> Type {
    match (ty, r.get_class()) {
        // If the type is provided
        (Some(t), _) => t,
        // If no type is provided, this should be a register spill for a
        // safepoint, so we only expect I64 (integer) registers.
        (None, RegClass::I64) => I64,
        _ => panic!("Unexpected register class!"),
    }
}

impl<M: ABIMachineImpl> ABIBody for ABIBodyImpl<M> {
    type I = M::I;

    fn temp_needed(&self) -> bool {
        self.sig.stack_ret_arg.is_some()
    }

    fn init(&mut self, maybe_tmp: Option<Writable<Reg>>) {
        if self.sig.stack_ret_arg.is_some() {
            assert!(maybe_tmp.is_some());
            self.ret_area_ptr = maybe_tmp;
        }
    }

    fn flags(&self) -> &settings::Flags {
        &self.flags
    }

    fn liveins(&self) -> Set<RealReg> {
        let mut set: Set<RealReg> = Set::empty();
        for &arg in &self.sig.args {
            if let ABIArg::Reg(r, ..) = arg {
                set.insert(r);
            }
        }
        set
    }

    fn liveouts(&self) -> Set<RealReg> {
        let mut set: Set<RealReg> = Set::empty();
        for &ret in &self.sig.rets {
            if let ABIArg::Reg(r, ..) = ret {
                set.insert(r);
            }
        }
        set
    }

    fn num_args(&self) -> usize {
        self.sig.args.len()
    }

    fn num_retvals(&self) -> usize {
        self.sig.rets.len()
    }

    fn num_stackslots(&self) -> usize {
        self.stackslots.len()
    }

    fn gen_copy_arg_to_reg(&self, idx: usize, into_reg: Writable<Reg>) -> Self::I {
        match &self.sig.args[idx] {
            // Extension mode doesn't matter (we're copying out, not in; we
            // ignore high bits by convention).
            &ABIArg::Reg(r, ty, _) => M::gen_move(into_reg, r.to_reg(), ty),
            &ABIArg::Stack(off, ty, _) => M::gen_load_stack(
                StackAMode::FPOffset(M::fp_to_arg_offset(self.call_conv, &self.flags) + off, ty),
                into_reg,
                ty,
            ),
        }
    }

    fn gen_copy_reg_to_retval(&self, idx: usize, from_reg: Writable<Reg>) -> Vec<Self::I> {
        let mut ret = Vec::new();
        match &self.sig.rets[idx] {
            &ABIArg::Reg(r, ty, ext) => {
                let from_bits = ty_bits(ty) as u8;
                let dest_reg = Writable::from_reg(r.to_reg());
                match (ext, from_bits) {
                    (ArgumentExtension::Uext, n) | (ArgumentExtension::Sext, n) if n < 64 => {
                        let signed = ext == ArgumentExtension::Sext;
                        ret.push(M::gen_extend(
                            dest_reg,
                            from_reg.to_reg(),
                            signed,
                            from_bits,
                            /* to_bits = */ 64,
                        ));
                    }
                    _ => ret.push(M::gen_move(dest_reg, from_reg.to_reg(), ty)),
                };
            }
            &ABIArg::Stack(off, mut ty, ext) => {
                let from_bits = ty_bits(ty) as u8;
                // Trash the from_reg; it should be its last use.
                match (ext, from_bits) {
                    (ArgumentExtension::Uext, n) | (ArgumentExtension::Sext, n) if n < 64 => {
                        assert_eq!(RegClass::I64, from_reg.to_reg().get_class());
                        let signed = ext == ArgumentExtension::Sext;
                        ret.push(M::gen_extend(
                            from_reg,
                            from_reg.to_reg(),
                            signed,
                            from_bits,
                            /* to_bits = */ 64,
                        ));
                        // Store the extended version.
                        ty = I64;
                    }
                    _ => {}
                };
                ret.push(M::gen_store_base_offset(
                    self.ret_area_ptr.unwrap().to_reg(),
                    off,
                    from_reg.to_reg(),
                    ty,
                ));
            }
        }
        ret
    }

    fn gen_retval_area_setup(&self) -> Option<Self::I> {
        if let Some(i) = self.sig.stack_ret_arg {
            let inst = self.gen_copy_arg_to_reg(i, self.ret_area_ptr.unwrap());
            trace!(
                "gen_retval_area_setup: inst {:?}; ptr reg is {:?}",
                inst,
                self.ret_area_ptr.unwrap().to_reg()
            );
            Some(inst)
        } else {
            trace!("gen_retval_area_setup: not needed");
            None
        }
    }

    fn gen_ret(&self) -> Self::I {
        M::gen_ret()
    }

    fn gen_epilogue_placeholder(&self) -> Self::I {
        M::gen_epilogue_placeholder()
    }

    fn set_num_spillslots(&mut self, slots: usize) {
        self.spillslots = Some(slots);
    }

    fn set_clobbered(&mut self, clobbered: Set<Writable<RealReg>>) {
        self.clobbered = clobbered;
    }

    /// Load from a stackslot.
    fn load_stackslot(
        &self,
        slot: StackSlot,
        offset: u32,
        ty: Type,
        into_reg: Writable<Reg>,
    ) -> Self::I {
        // Offset from beginning of stackslot area, which is at nominal SP (see
        // [MemArg::NominalSPOffset] for more details on nominal SP tracking).
        let stack_off = self.stackslots[slot.as_u32() as usize] as i64;
        let sp_off: i64 = stack_off + (offset as i64);
        trace!("load_stackslot: slot {} -> sp_off {}", slot, sp_off);
        M::gen_load_stack(StackAMode::NominalSPOffset(sp_off, ty), into_reg, ty)
    }

    /// Store to a stackslot.
    fn store_stackslot(&self, slot: StackSlot, offset: u32, ty: Type, from_reg: Reg) -> Self::I {
        // Offset from beginning of stackslot area, which is at nominal SP (see
        // [MemArg::NominalSPOffset] for more details on nominal SP tracking).
        let stack_off = self.stackslots[slot.as_u32() as usize] as i64;
        let sp_off: i64 = stack_off + (offset as i64);
        trace!("store_stackslot: slot {} -> sp_off {}", slot, sp_off);
        M::gen_store_stack(StackAMode::NominalSPOffset(sp_off, ty), from_reg, ty)
    }

    /// Produce an instruction that computes a stackslot address.
    fn stackslot_addr(&self, slot: StackSlot, offset: u32, into_reg: Writable<Reg>) -> Self::I {
        // Offset from beginning of stackslot area, which is at nominal SP (see
        // [MemArg::NominalSPOffset] for more details on nominal SP tracking).
        let stack_off = self.stackslots[slot.as_u32() as usize] as i64;
        let sp_off: i64 = stack_off + (offset as i64);
        M::gen_get_stack_addr(StackAMode::NominalSPOffset(sp_off, I8), into_reg, I8)
    }

    /// Load from a spillslot.
    fn load_spillslot(&self, slot: SpillSlot, ty: Type, into_reg: Writable<Reg>) -> Self::I {
        // Offset from beginning of spillslot area, which is at nominal SP + stackslots_size.
        let islot = slot.get() as i64;
        let spill_off = islot * 8; // FIXME: 64-bit machine assumed.
        let sp_off = self.stackslots_size as i64 + spill_off;
        trace!("load_spillslot: slot {:?} -> sp_off {}", slot, sp_off);
        M::gen_load_stack(StackAMode::NominalSPOffset(sp_off, ty), into_reg, ty)
    }

    /// Store to a spillslot.
    fn store_spillslot(&self, slot: SpillSlot, ty: Type, from_reg: Reg) -> Self::I {
        // Offset from beginning of spillslot area, which is at nominal SP + stackslots_size.
        let islot = slot.get() as i64;
        let spill_off = islot * 8; // FIXME: 64-bit machine assumed.
        let sp_off = self.stackslots_size as i64 + spill_off;
        trace!("store_spillslot: slot {:?} -> sp_off {}", slot, sp_off);
        M::gen_store_stack(StackAMode::NominalSPOffset(sp_off, ty), from_reg, ty)
    }

    fn spillslots_to_stack_map(
        &self,
        slots: &[SpillSlot],
        state: &<Self::I as MachInstEmit>::State,
    ) -> StackMap {
        let virtual_sp_offset = M::get_virtual_sp_offset_from_state(state);
        let nominal_sp_to_fp = M::get_nominal_sp_to_fp(state);
        assert!(virtual_sp_offset >= 0);
        trace!(
            "spillslots_to_stackmap: slots = {:?}, state = {:?}",
            slots,
            state
        );
        let map_size = (virtual_sp_offset + nominal_sp_to_fp) as u32;
        let map_words = (map_size + 7) / 8; // FIXME: 64-bit machine assumed.
        let mut bits = core::iter::repeat(false)
            .take(map_words as usize)
            .collect::<Vec<bool>>();

        let first_spillslot_word = ((self.stackslots_size + virtual_sp_offset as u32) / 8) as usize;
        for &slot in slots {
            let slot = slot.get() as usize;
            bits[first_spillslot_word + slot] = true;
        }

        StackMap::from_slice(&bits[..])
    }

    fn gen_prologue(&mut self) -> Vec<Self::I> {
        let mut insts = vec![];
        if !self.call_conv.extends_baldrdash() {
            // set up frame
            insts.extend(M::gen_prologue_frame_setup().into_iter());
        }

        let mut total_stacksize = self.stackslots_size + 8 * self.spillslots.unwrap() as u32;
        if self.call_conv.extends_baldrdash() {
            debug_assert!(
                !self.flags.enable_probestack(),
                "baldrdash does not expect cranelift to emit stack probes"
            );
            // FIXME: 64-bit machine assumed.
            total_stacksize += self.flags.baldrdash_prologue_words() as u32 * 8;
        }
        let total_stacksize = (total_stacksize + 15) & !15; // 16-align the stack.

        let mut total_sp_adjust = 0;

        if !self.call_conv.extends_baldrdash() {
            // Leaf functions with zero stack don't need a stack check if one's
            // specified, otherwise always insert the stack check.
            if total_stacksize > 0 || !self.is_leaf {
                if let Some((reg, stack_limit_load)) = &self.stack_limit {
                    insts.extend_from_slice(stack_limit_load);
                    self.insert_stack_check(*reg, total_stacksize, &mut insts);
                }
            }
            if total_stacksize > 0 {
                total_sp_adjust += total_stacksize as u64;
            }
        }

        // N.B.: "nominal SP", which we use to refer to stackslots and
        // spillslots, is defined to be equal to the stack pointer at this point
        // in the prologue.
        //
        // If we push any clobbers below, we emit a virtual-SP adjustment
        // meta-instruction so that the nominal SP references behave as if SP
        // were still at this point. See documentation for
        // [crate::machinst::abi_impl](this module) for more details on
        // stackframe layout and nominal SP maintenance.

        if total_sp_adjust > 0 {
            // sub sp, sp, #total_stacksize
            let adj = total_sp_adjust as i64;
            insts.extend(M::gen_sp_reg_adjust(-adj));
        }

        // Save clobbered registers.
        let (clobber_size, clobber_insts) = M::gen_clobber_save(self.call_conv, &self.clobbered);
        insts.extend(clobber_insts);

        if clobber_size > 0 {
            insts.push(M::gen_nominal_sp_adj(clobber_size as i64));
        }

        self.total_frame_size = Some(total_stacksize);
        insts
    }

    fn gen_epilogue(&self) -> Vec<M::I> {
        let mut insts = vec![];

        // Restore clobbered registers.
        insts.extend(M::gen_clobber_restore(self.call_conv, &self.clobbered));

        // N.B.: we do *not* emit a nominal SP adjustment here, because (i) there will be no
        // references to nominal SP offsets before the return below, and (ii) the instruction
        // emission tracks running SP offset linearly (in straight-line order), not according to
        // the CFG, so early returns in the middle of function bodies would cause an incorrect
        // offset for the rest of the body.

        if !self.call_conv.extends_baldrdash() {
            insts.extend(M::gen_epilogue_frame_restore());
            insts.push(M::gen_ret());
        }

        debug!("Epilogue: {:?}", insts);
        insts
    }

    fn frame_size(&self) -> u32 {
        self.total_frame_size
            .expect("frame size not computed before prologue generation")
    }

    fn stack_args_size(&self) -> u32 {
        self.sig.stack_arg_space as u32
    }

    fn get_spillslot_size(&self, rc: RegClass, ty: Type) -> u32 {
        M::get_spillslot_size(rc, ty)
    }

    fn gen_spill(&self, to_slot: SpillSlot, from_reg: RealReg, ty: Option<Type>) -> Self::I {
        let ty = ty_from_ty_hint_or_reg_class(from_reg.to_reg(), ty);
        self.store_spillslot(to_slot, ty, from_reg.to_reg())
    }

    fn gen_reload(
        &self,
        to_reg: Writable<RealReg>,
        from_slot: SpillSlot,
        ty: Option<Type>,
    ) -> Self::I {
        let ty = ty_from_ty_hint_or_reg_class(to_reg.to_reg().to_reg(), ty);
        self.load_spillslot(from_slot, ty, to_reg.map(|r| r.to_reg()))
    }
}

fn abisig_to_uses_and_defs<M: ABIMachineImpl>(sig: &ABISig) -> (Vec<Reg>, Vec<Writable<Reg>>) {
    // Compute uses: all arg regs.
    let mut uses = Vec::new();
    for arg in &sig.args {
        match arg {
            &ABIArg::Reg(reg, ..) => uses.push(reg.to_reg()),
            _ => {}
        }
    }

    // Compute defs: all retval regs, and all caller-save (clobbered) regs.
    let mut defs = M::get_caller_saves(sig.call_conv);
    for ret in &sig.rets {
        match ret {
            &ABIArg::Reg(reg, ..) => defs.push(Writable::from_reg(reg.to_reg())),
            _ => {}
        }
    }

    (uses, defs)
}

/// ABI object for a callsite.
pub struct ABICallImpl<M: ABIMachineImpl> {
    /// The called function's signature.
    sig: ABISig,
    /// All uses for the callsite, i.e., function args.
    uses: Vec<Reg>,
    /// All defs for the callsite, i.e., return values and caller-saves.
    defs: Vec<Writable<Reg>>,
    /// Call destination.
    dest: CallDest,
    /// Location of callsite.
    loc: ir::SourceLoc,
    /// Actuall call opcode; used to distinguish various types of calls.
    opcode: ir::Opcode,

    _mach: PhantomData<M>,
}

/// Destination for a call.
#[derive(Debug, Clone)]
pub enum CallDest {
    /// Call to an ExtName (named function symbol).
    ExtName(ir::ExternalName, RelocDistance),
    /// Indirect call to a function pointer in a register.
    Reg(Reg),
}

impl<M: ABIMachineImpl> ABICallImpl<M> {
    /// Create a callsite ABI object for a call directly to the specified function.
    pub fn from_func(
        sig: &ir::Signature,
        extname: &ir::ExternalName,
        dist: RelocDistance,
        loc: ir::SourceLoc,
    ) -> CodegenResult<ABICallImpl<M>> {
        let sig = ABISig::from_func_sig::<M>(sig)?;
        let (uses, defs) = abisig_to_uses_and_defs::<M>(&sig);
        Ok(ABICallImpl {
            sig,
            uses,
            defs,
            dest: CallDest::ExtName(extname.clone(), dist),
            loc,
            opcode: ir::Opcode::Call,
            _mach: PhantomData,
        })
    }

    /// Create a callsite ABI object for a call to a function pointer with the
    /// given signature.
    pub fn from_ptr(
        sig: &ir::Signature,
        ptr: Reg,
        loc: ir::SourceLoc,
        opcode: ir::Opcode,
    ) -> CodegenResult<ABICallImpl<M>> {
        let sig = ABISig::from_func_sig::<M>(sig)?;
        let (uses, defs) = abisig_to_uses_and_defs::<M>(&sig);
        Ok(ABICallImpl {
            sig,
            uses,
            defs,
            dest: CallDest::Reg(ptr),
            loc,
            opcode,
            _mach: PhantomData,
        })
    }
}

fn adjust_stack_and_nominal_sp<M: ABIMachineImpl, C: LowerCtx<I = M::I>>(
    ctx: &mut C,
    off: u64,
    is_sub: bool,
) {
    if off == 0 {
        return;
    }
    let off = off as i64;
    let amt = if is_sub { -off } else { off };
    for inst in M::gen_sp_reg_adjust(amt) {
        ctx.emit(inst);
    }
    ctx.emit(M::gen_nominal_sp_adj(-amt));
}

impl<M: ABIMachineImpl> ABICall for ABICallImpl<M> {
    type I = M::I;

    fn num_args(&self) -> usize {
        if self.sig.stack_ret_arg.is_some() {
            self.sig.args.len() - 1
        } else {
            self.sig.args.len()
        }
    }

    fn emit_stack_pre_adjust<C: LowerCtx<I = Self::I>>(&self, ctx: &mut C) {
        let off = self.sig.stack_arg_space + self.sig.stack_ret_space;
        adjust_stack_and_nominal_sp::<M, C>(ctx, off as u64, /* is_sub = */ true)
    }

    fn emit_stack_post_adjust<C: LowerCtx<I = Self::I>>(&self, ctx: &mut C) {
        let off = self.sig.stack_arg_space + self.sig.stack_ret_space;
        adjust_stack_and_nominal_sp::<M, C>(ctx, off as u64, /* is_sub = */ false)
    }

    fn emit_copy_reg_to_arg<C: LowerCtx<I = Self::I>>(
        &self,
        ctx: &mut C,
        idx: usize,
        from_reg: Reg,
    ) {
        match &self.sig.args[idx] {
            &ABIArg::Reg(reg, ty, ext)
                if ext != ir::ArgumentExtension::None && ty_bits(ty) < 64 =>
            {
                assert_eq!(RegClass::I64, reg.get_class());
                let signed = match ext {
                    ir::ArgumentExtension::Uext => false,
                    ir::ArgumentExtension::Sext => true,
                    _ => unreachable!(),
                };
                ctx.emit(M::gen_extend(
                    Writable::from_reg(reg.to_reg()),
                    from_reg,
                    signed,
                    ty_bits(ty) as u8,
                    64,
                ));
            }
            &ABIArg::Reg(reg, ty, _) => {
                ctx.emit(M::gen_move(Writable::from_reg(reg.to_reg()), from_reg, ty));
            }
            &ABIArg::Stack(off, mut ty, ext) => {
                if ext != ir::ArgumentExtension::None && ty_bits(ty) < 64 {
                    assert_eq!(RegClass::I64, from_reg.get_class());
                    let signed = match ext {
                        ir::ArgumentExtension::Uext => false,
                        ir::ArgumentExtension::Sext => true,
                        _ => unreachable!(),
                    };
                    // Extend in place in the source register. Our convention is to
                    // treat high bits as undefined for values in registers, so this
                    // is safe, even for an argument that is nominally read-only.
                    ctx.emit(M::gen_extend(
                        Writable::from_reg(from_reg),
                        from_reg,
                        signed,
                        ty_bits(ty) as u8,
                        64,
                    ));
                    // Store the extended version.
                    ty = I64;
                }
                ctx.emit(M::gen_store_stack(
                    StackAMode::SPOffset(off, ty),
                    from_reg,
                    ty,
                ));
            }
        }
    }

    fn emit_copy_retval_to_reg<C: LowerCtx<I = Self::I>>(
        &self,
        ctx: &mut C,
        idx: usize,
        into_reg: Writable<Reg>,
    ) {
        match &self.sig.rets[idx] {
            // Extension mode doesn't matter because we're copying out, not in,
            // and we ignore high bits in our own registers by convention.
            &ABIArg::Reg(reg, ty, _) => ctx.emit(M::gen_move(into_reg, reg.to_reg(), ty)),
            &ABIArg::Stack(off, ty, _) => {
                let ret_area_base = self.sig.stack_arg_space;
                ctx.emit(M::gen_load_stack(
                    StackAMode::SPOffset(off + ret_area_base, ty),
                    into_reg,
                    ty,
                ));
            }
        }
    }

    fn emit_call<C: LowerCtx<I = Self::I>>(&mut self, ctx: &mut C) {
        let (uses, defs) = (
            mem::replace(&mut self.uses, Default::default()),
            mem::replace(&mut self.defs, Default::default()),
        );
        if let Some(i) = self.sig.stack_ret_arg {
            let rd = ctx.alloc_tmp(RegClass::I64, I64);
            let ret_area_base = self.sig.stack_arg_space;
            ctx.emit(M::gen_get_stack_addr(
                StackAMode::SPOffset(ret_area_base, I8),
                rd,
                I8,
            ));
            self.emit_copy_reg_to_arg(ctx, i, rd.to_reg());
        }
        for (is_safepoint, inst) in
            M::gen_call(&self.dest, uses, defs, self.loc, self.opcode).into_iter()
        {
            if is_safepoint {
                ctx.emit_safepoint(inst);
            } else {
                ctx.emit(inst);
            }
        }
    }
}