#![allow(dead_code)] // TODO: remove

mod consts;
mod provider;
mod register_alloc;

pub use self::{
    consts::{FuncLocalConsts, FuncLocalConstsIter},
    provider::{ProviderStack, TaggedProvider},
    register_alloc::{DefragRegister, RegisterAlloc},
};
use super::TypedValue;
use crate::{
    engine::{
        bytecode2::{Register, RegisterSpan},
        func_builder::TranslationErrorInner,
        Instr,
        Provider,
        TranslationError,
        UntypedProvider,
    },
    FuncType,
};
use alloc::vec::Vec;
use wasmi_core::UntypedValue;

/// Typed inputs to `wasmi` bytecode instructions.
///
/// Either a [`Register`] or a constant [`UntypedValue`].
///
/// # Note
///
/// The [`TypedProvider`] is used primarily during translation of a `wasmi`
/// function where types of constant values play an important role.
pub type TypedProvider = Provider<TypedValue>;

impl TypedProvider {
    /// Converts the [`TypedProvider`] to a resolved [`UntypedProvider`].
    pub fn into_untyped(self) -> UntypedProvider {
        match self {
            Self::Register(register) => UntypedProvider::Register(register),
            Self::Const(value) => UntypedProvider::Const(UntypedValue::from(value)),
        }
    }
}

/// The value stack.
#[derive(Debug, Default)]
pub struct ValueStack {
    providers: ProviderStack,
    reg_alloc: RegisterAlloc,
    consts: FuncLocalConsts,
}

impl ValueStack {
    /// Resets the [`ValueStack`].
    pub fn reset(&mut self) {
        self.providers.reset();
        self.reg_alloc.reset();
        self.consts.reset();
    }

    /// Pops [`Provider`] from the [`ValueStack`] until it has the given stack `height`.
    pub fn trunc(&mut self, height: usize) {
        assert!(height <= self.height());
        while self.height() != height {
            self.pop();
        }
    }

    /// Adjusts the [`ValueStack`] given the [`FuncType`] of the call.
    ///
    /// - Returns the [`RegisterSpan`] for the `call` results.
    /// - The `provider_buffer` will hold all [`Provider`] call parameters.
    /// - The `params_buffer` will hold all call parameters converted to [`Register`]. \
    ///   Any constant value parameter will be allocated as function local constant.
    ///
    /// # Note
    ///
    /// Both `provider_buffer` and `params_buffer` will be cleared before this operation.
    ///
    /// # Errors
    ///
    /// - If not enough call parameters are on the [`ValueStack`].
    /// - If too many function local constants are being registered as call parameters.
    /// - If too many registers are registered as call results.
    pub fn adjust_for_call(
        &mut self,
        func_type: &FuncType,
        provider_buffer: &mut Vec<TypedProvider>,
    ) -> Result<RegisterSpan, TranslationError> {
        let (params, results) = func_type.params_results();
        self.pop_n(params.len(), provider_buffer);
        let results = self.push_dynamic_n(results.len())?;
        Ok(results)
    }

    /// Returns the number of [`Provider`] on the [`ValueStack`].
    ///
    /// # Note
    ///
    /// This is the same as the height of the [`ValueStack`].
    pub fn height(&self) -> usize {
        self.providers.len()
    }

    /// Returns the number of registers allocated by the [`RegisterAlloc`].
    pub fn len_registers(&self) -> u16 {
        // The addition won't overflow since both operands are in the range of `0..i16::MAX`.
        self.consts.len_consts() + self.reg_alloc.len_registers()
    }

    /// Registers an `amount` of function inputs or local variables.
    ///
    /// # Errors
    ///
    /// If too many registers have been registered.
    ///
    /// # Panics
    ///
    /// If the [`RegisterAlloc`] is not in its initialization phase.
    pub fn register_locals(&mut self, amount: u32) -> Result<(), TranslationError> {
        self.reg_alloc.register_locals(amount)
    }

    /// Finishes the local variable registration phase.
    ///
    /// # Note
    ///
    /// After this operation no local variable can be registered anymore.
    /// However, it is then possible to push and pop dynamic and storage registers to the stack.
    pub fn finish_register_locals(&mut self) {
        self.reg_alloc.finish_register_locals()
    }

    /// Allocates a new function local constant value and returns its [`Register`].
    ///
    /// # Note
    ///
    /// Constant values allocated this way are deduplicated and return shared [`Register`].
    pub fn alloc_const<T>(&mut self, value: T) -> Result<Register, TranslationError>
    where
        T: Into<UntypedValue>,
    {
        self.consts.alloc(value.into())
    }

    /// Returns the allocated function local constant values in reversed allocation order.
    ///
    /// # Note
    ///
    /// Upon calling a function all of its function local constant values are
    /// inserted into the current execution call frame in reversed allocation order
    /// and accessed via negative [`Register`] index where the 0 index is referring
    /// to the first function local and the -1 index is referring to the first
    /// allocated function local constant value.
    pub fn func_local_consts(&self) -> FuncLocalConstsIter {
        self.consts.iter()
    }

    /// Pushes a constant value to the [`ProviderStack`].
    pub fn push_const<T>(&mut self, value: T)
    where
        T: Into<TypedValue>,
    {
        self.providers.push_const(value)
    }

    /// Pushes the given [`TypedProvider`] to the [`ValueStack`].
    ///
    /// # Note
    ///
    /// This is a convenice method for [`ValueStack::push_register`] and [`ValueStack::push_const`].
    pub fn push_provider(&mut self, provider: TypedProvider) -> Result<(), TranslationError> {
        match provider {
            Provider::Register(register) => self.push_register(register)?,
            Provider::Const(value) => self.push_const(value),
        }
        Ok(())
    }

    /// Pushes the given [`Register`] to the [`ValueStack`].
    ///
    /// # Errors
    ///
    /// If too many registers have been registered.
    pub fn push_register(&mut self, reg: Register) -> Result<(), TranslationError> {
        if self.reg_alloc.is_dynamic(reg) {
            self.reg_alloc.push_dynamic()?;
            self.providers.push_dynamic(reg);
            return Ok(());
        }
        if self.reg_alloc.is_storage(reg) {
            // self.reg_alloc.push_storage()?; TODO
            self.providers.push_storage(reg);
            // return Ok(())
            todo!() // see above lines
        }
        self.providers.push_local(reg);
        Ok(())
    }

    /// Pushes a [`Register`] to the [`ValueStack`] referring to a function parameter or local variable.
    ///
    /// # Errors
    ///
    /// If too many registers have been registered.
    pub fn push_local(&mut self, local_index: u32) -> Result<Register, TranslationError> {
        let index = i16::try_from(local_index)
            .ok()
            .filter(|&value| value as u16 <= self.reg_alloc.len_locals())
            .ok_or_else(|| TranslationError::new(TranslationErrorInner::RegisterOutOfBounds))?;
        let reg = Register::from_i16(index);
        self.providers.push_local(reg);
        Ok(reg)
    }

    /// Pushes a dynamically allocated [`Register`] to the [`ValueStack`].
    ///
    /// # Errors
    ///
    /// If too many registers have been registered.
    pub fn push_dynamic(&mut self) -> Result<Register, TranslationError> {
        let reg = self.reg_alloc.push_dynamic()?;
        self.providers.push_dynamic(reg);
        Ok(reg)
    }

    /// Pops the top-most [`Provider`] from the [`ValueStack`].
    pub fn pop(&mut self) -> TypedProvider {
        self.reg_alloc.pop_provider(self.providers.pop())
    }

    /// Pops the top-most [`Provider`] from the [`ValueStack`] as [`Register`].
    ///
    /// # Note
    ///
    /// Conversion from [`Provider`] to [`Register`] is done by allocating function local constant values.
    pub fn pop_as_register(&mut self) -> Result<Register, TranslationError> {
        match self.pop() {
            Provider::Register(register) => Ok(register),
            Provider::Const(value) => self.alloc_const(value),
        }
    }

    /// Peeks the top-most [`Provider`] from the [`ValueStack`].
    pub fn peek(&self) -> TypedProvider {
        match self.providers.peek() {
            TaggedProvider::Local(register)
            | TaggedProvider::Dynamic(register)
            | TaggedProvider::Storage(register) => TypedProvider::Register(register),
            TaggedProvider::Const(value) => TypedProvider::Const(value),
        }
    }

    /// Pops the two top-most [`Provider`] from the [`ValueStack`].
    pub fn pop2(&mut self) -> (TypedProvider, TypedProvider) {
        let rhs = self.pop();
        let lhs = self.pop();
        (lhs, rhs)
    }

    /// Pops the two top-most [`Provider`] from the [`ValueStack`] as [`Register`].
    ///
    /// # Note
    ///
    /// Conversion from [`Provider`] to [`Register`] is done by allocating function local constant values.
    pub fn pop2_as_registers(&mut self) -> Result<(Register, Register), TranslationError> {
        let rhs = self.pop_as_register()?;
        let lhs = self.pop_as_register()?;
        Ok((lhs, rhs))
    }

    /// Pops the three top-most [`Provider`] from the [`ValueStack`].
    pub fn pop3(&mut self) -> (TypedProvider, TypedProvider, TypedProvider) {
        let (v1, v2) = self.pop2();
        let v0 = self.pop();
        (v0, v1, v2)
    }

    /// Pops the three top-most [`Provider`] from the [`ValueStack`] as [`Register`].
    ///
    /// # Note
    ///
    /// Conversion from [`Provider`] to [`Register`] is done by allocating function local constant values.
    pub fn pop3_as_registers(
        &mut self,
    ) -> Result<(Register, Register, Register), TranslationError> {
        let (v1, v2) = self.pop2_as_registers()?;
        let v0 = self.pop_as_register()?;
        Ok((v0, v1, v2))
    }

    /// Popn the `n` top-most [`Provider`] from the [`ValueStack`] and store them in `result`.
    ///
    /// # Note
    ///
    /// - The top-most [`Provider`] will be the n-th item in `result`.
    /// - The `result` [`Vec`] will be cleared before refilled.
    pub fn pop_n(&mut self, n: usize, result: &mut Vec<TypedProvider>) {
        result.clear();
        for _ in 0..n {
            let provider = self.pop();
            result.push(provider);
        }
        result[..].reverse()
    }

    /// Popn the `n` top-most [`Provider`] from the [`ValueStack`] and convert them to [`Register`].
    ///
    /// # Note
    ///
    /// - Conversion from [`Provider`] to [`Register`] is done by allocating function local constant values.
    /// - Both `provider_buffer` and `params_buffer` will be cleared before this operation.
    ///
    /// # Errors
    ///
    /// - If there are too few providers on the [`ValueStack`].
    /// - If too many function local constants are being registered as call parameters.
    pub fn pop_n_as_registers(
        &mut self,
        n: usize,
        provider_buffer: &mut Vec<TypedProvider>,
        params_buffer: &mut Vec<Register>,
    ) -> Result<(), TranslationError> {
        self.pop_n(n, provider_buffer);
        params_buffer.clear();
        for provider in provider_buffer.iter().copied() {
            let register = match provider {
                Provider::Register(register) => register,
                Provider::Const(value) => self.consts.alloc(value.into())?,
            };
            params_buffer.push(register);
        }
        Ok(())
    }

    /// Peeks the `n` top-most [`Provider`] from the [`ValueStack`] and store them in `result`.
    ///
    /// # Note
    ///
    /// - The top-most [`Provider`] will be the n-th item in `result`.
    /// - The `result` [`Vec`] will be cleared before refilled.
    pub fn peek_n(&mut self, n: usize, result: &mut Vec<TypedProvider>) {
        result.clear();
        let peeked = self.providers.peek_n(n);
        for provider in peeked {
            let provider = match *provider {
                TaggedProvider::Local(register)
                | TaggedProvider::Dynamic(register)
                | TaggedProvider::Storage(register) => TypedProvider::Register(register),
                TaggedProvider::Const(value) => TypedProvider::Const(value),
            };
            result.push(provider);
        }
    }

    /// Peeks the `n` top-most [`Provider`] from the [`ValueStack`] and store them in `result`.
    ///
    /// # Note
    ///
    /// - The top-most [`Provider`] will be the n-th item in `result`.
    /// - The `result` [`Vec`] will be cleared before refilled.
    pub fn peek_n_as_registers(
        &mut self,
        n: usize,
        results: &mut Vec<Register>,
    ) -> Result<(), TranslationError> {
        results.clear();
        let peeked = self.providers.peek_n(n);
        for provider in peeked {
            let register = match *provider {
                TaggedProvider::Local(register)
                | TaggedProvider::Dynamic(register)
                | TaggedProvider::Storage(register) => register,
                TaggedProvider::Const(value) => self.consts.alloc(value.into())?,
            };
            results.push(register);
        }
        Ok(())
    }

    /// Pushes the given `providers` into the [`ValueStack`].
    ///
    /// # Errors
    ///
    /// If too many registers have been registered.
    pub fn push_n(&mut self, providers: &[TypedProvider]) -> Result<(), TranslationError> {
        for provider in providers {
            match *provider {
                TypedProvider::Register(register) => self.push_register(register)?,
                TypedProvider::Const(value) => self.push_const(value),
            }
        }
        Ok(())
    }

    /// Returns a [`RegisterSpan`] of `n` registers as if they were dynamically allocated.
    ///
    /// # Note
    ///
    /// - This procedure pushes dynamic [`Register`] onto the [`ValueStack`].
    /// - This is primarily used to allocate branch parameters for control
    ///    flow frames such as Wasm `block`, `loop` and `if` as well as for
    ///    instructions that may return multiple values such as `call`.
    ///
    /// # Errors
    ///
    /// If this procedure would allocate more registers than are available.
    pub fn push_dynamic_n(&mut self, n: usize) -> Result<RegisterSpan, TranslationError> {
        let registers = self.reg_alloc.push_dynamic_n(n)?;
        for register in registers.iter(n) {
            self.providers.push_dynamic(register);
        }
        Ok(registers)
    }

    /// Returns a [`RegisterSpan`] of `n` registers as if they were dynamically allocated.
    ///
    /// # Note
    ///
    /// - This procedure does not push anything onto the [`ValueStack`].
    /// - This is primarily used to allocate branch parameters for control
    ///    flow frames such as Wasm `block`, `loop` and `if`.
    ///
    /// # Errors
    ///
    /// If this procedure would allocate more registers than are available.
    pub fn peek_dynamic_n(&mut self, n: usize) -> Result<RegisterSpan, TranslationError> {
        let registers = self.reg_alloc.push_dynamic_n(n)?;
        self.reg_alloc.pop_dynamic_n(n);
        Ok(registers)
    }

    /// Registers the [`Instr`] user for [`Register`] if `reg` is allocated in storage space.
    ///
    /// # Note
    ///
    /// This is required in order to update [`Register`] indices of storage space
    /// allocated registers after register allocation is finished.
    pub fn register_user(&mut self, reg: Register, user: Instr) {
        self.reg_alloc.register_user(reg, user)
    }

    /// Defragments the allocated registers space.
    ///
    /// # Note
    ///
    /// This is needed because dynamically allocated registers and storage space allocated
    /// registers do not have consecutive index spaces for technical reasons. This is why we
    /// store the definition site and users of storage space allocated registers so that we
    /// can defrag exactly those registers and make the allocated register space compact.
    pub fn defrag(&mut self, state: &mut impl DefragRegister) {
        self.reg_alloc.defrag(state)
    }
}
