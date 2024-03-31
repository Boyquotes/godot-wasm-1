use std::any::Any;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::{fmt, mem, ptr};

use anyhow::{bail, Error};
use cfg_if::cfg_if;
use godot::prelude::*;
use once_cell::sync::OnceCell;
use parking_lot::{lock_api::RawMutex as RawMutexTrait, Mutex, RawMutex};
use rayon::prelude::*;
use scopeguard::guard;
#[cfg(feature = "wasi")]
use wasi_common::WasiCtx;
#[cfg(feature = "component-model")]
use wasmtime::component::Instance as InstanceComp;
#[cfg(feature = "wasi-preview2")]
use wasmtime::component::ResourceTable;
#[cfg(feature = "wasi")]
use wasmtime::Linker;
#[cfg(feature = "memory-limiter")]
use wasmtime::ResourceLimiter;
use wasmtime::{
    AsContextMut, Extern, Func, FuncType, Instance as InstanceWasm, Memory, Store, StoreContextMut,
    ValRaw,
};
#[cfg(feature = "wasi-preview2")]
use wasmtime_wasi::preview2::{WasiCtx as WasiCtxPv2, WasiView};
#[cfg(feature = "wasi")]
use wasmtime_wasi::sync::{add_to_linker, WasiCtxBuilder};

use crate::rw_struct::{read_struct, write_struct};
#[cfg(feature = "wasi")]
use crate::wasi_ctx::stdio::{
    BlockWritePipe, ByteBufferReadPipe, InnerStdin, LineWritePipe, OuterStdin, UnbufferedWritePipe,
};
#[cfg(feature = "wasi")]
use crate::wasi_ctx::WasiContext;
use crate::wasm_config::Config;
#[cfg(any(feature = "object-registry-compat", feature = "object-registry-extern"))]
use crate::wasm_config::ExternBindingType;
#[cfg(feature = "wasi")]
use crate::wasm_config::{PipeBindingType, PipeBufferType};
use crate::wasm_engine::{ModuleData, ModuleType, WasmModule, ENGINE};
#[cfg(feature = "object-registry-extern")]
use crate::wasm_externref::Funcs as ExternrefFuncs;
#[cfg(feature = "object-registry-compat")]
use crate::wasm_objregistry::{Funcs as ObjregistryFuncs, ObjectRegistry};
#[cfg(feature = "object-registry-extern")]
use crate::wasm_util::EXTERNREF_MODULE;
#[cfg(feature = "object-registry-compat")]
use crate::wasm_util::OBJREGISTRY_MODULE;
use crate::wasm_util::{
    config_store_common, from_raw, option_to_variant, to_raw, variant_to_option, HostModuleCache,
    PhantomProperty, SendSyncWrapper, VariantDispatch, MEMORY_EXPORT,
};
use crate::{bail_with_site, site_context};

#[derive(GodotClass)]
#[class(base=RefCounted, init, tool)]
pub struct WasmInstance {
    base: Base<RefCounted>,
    data: OnceCell<InstanceData<StoreData>>,
    memory: Option<Memory>,

    #[var(get = get_module)]
    #[allow(dead_code)]
    module: PhantomProperty<Option<Gd<WasmModule>>>,
}

pub struct InstanceData<T> {
    pub store: Mutex<Store<T>>,
    pub instance: InstanceType,
    pub module: Gd<WasmModule>,

    #[cfg(feature = "wasi")]
    pub wasi_stdin: Option<Arc<InnerStdin<dyn Any + Send + Sync>>>,
}

pub enum InstanceType {
    Core(InstanceWasm),
    #[cfg(feature = "component-model")]
    Component(InstanceComp),
}

impl InstanceType {
    pub fn get_core(&self) -> Result<&InstanceWasm, Error> {
        #[allow(irrefutable_let_patterns)]
        if let Self::Core(m) = self {
            Ok(m)
        } else {
            bail!("Instance is a component")
        }
    }

    #[allow(dead_code)]
    #[cfg(feature = "component-model")]
    pub fn get_component(&self) -> Result<&InstanceComp, Error> {
        if let Self::Component(m) = self {
            Ok(m)
        } else {
            bail!("Instance is a component")
        }
    }
}

pub struct InnerLock {
    mutex_raw: *const RawMutex,
}

// SAFETY: Store data is safely contained within instance data?
unsafe impl Send for InnerLock {}
unsafe impl Sync for InnerLock {}

impl Default for InnerLock {
    fn default() -> Self {
        Self {
            mutex_raw: ptr::null(),
        }
    }
}

pub struct StoreData {
    inner_lock: InnerLock,
    pub error_signal: Option<String>,

    #[cfg(feature = "epoch-timeout")]
    pub epoch_timeout: u64,
    #[cfg(feature = "epoch-timeout")]
    pub epoch_autoreset: bool,

    #[cfg(feature = "memory-limiter")]
    pub memory_limits: MemoryLimit,

    #[cfg(feature = "object-registry-compat")]
    pub object_registry: Option<ObjectRegistry>,

    #[cfg(feature = "wasi")]
    pub wasi_ctx: MaybeWasi,
}

// SAFETY: Store data is safely contained within instance data?
unsafe impl Send for StoreData {}
unsafe impl Sync for StoreData {}

impl AsRef<Self> for StoreData {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl AsMut<Self> for StoreData {
    fn as_mut(&mut self) -> &mut Self {
        self
    }
}

impl AsRef<InnerLock> for StoreData {
    fn as_ref(&self) -> &InnerLock {
        &self.inner_lock
    }
}

impl AsMut<InnerLock> for StoreData {
    fn as_mut(&mut self) -> &mut InnerLock {
        &mut self.inner_lock
    }
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            inner_lock: InnerLock::default(),
            error_signal: None,

            #[cfg(feature = "epoch-timeout")]
            epoch_timeout: 0,
            #[cfg(feature = "epoch-timeout")]
            epoch_autoreset: false,

            #[cfg(feature = "memory-limiter")]
            memory_limits: MemoryLimit::default(),

            #[cfg(feature = "object-registry-compat")]
            object_registry: None,

            #[cfg(feature = "wasi")]
            wasi_ctx: MaybeWasi::NoCtx,
        }
    }
}

pub enum MaybeWasi {
    NoCtx,
    Preview1(WasiCtx),
    #[cfg(feature = "wasi-preview2")]
    Preview2(WasiCtxPv2, ResourceTable),
}

#[cfg(feature = "wasi-preview2")]
impl WasiView for StoreData {
    fn table(&mut self) -> &mut ResourceTable {
        match &mut self.wasi_ctx {
            MaybeWasi::Preview2(_, tbl) => tbl,
            _ => panic!("Requested WASI Preview 2 interface while none set, this is a bug"),
        }
    }

    fn ctx(&mut self) -> &mut WasiCtxPv2 {
        match &mut self.wasi_ctx {
            MaybeWasi::Preview2(ctx, _) => ctx,
            _ => panic!("Requested WASI Preview 2 interface while none set, this is a bug"),
        }
    }
}

#[cfg(feature = "memory-limiter")]
pub struct MemoryLimit {
    pub max_memory: u64,
    pub max_table_entries: u64,
}

#[cfg(feature = "memory-limiter")]
impl Default for MemoryLimit {
    fn default() -> Self {
        Self {
            max_memory: u64::MAX,
            max_table_entries: u64::MAX,
        }
    }
}

#[cfg(feature = "memory-limiter")]
impl MemoryLimit {
    pub fn from_config(config: &Config) -> Self {
        let mut ret = Self::default();
        if let Some(v) = config.max_memory {
            ret.max_memory = v;
        }
        if let Some(v) = config.max_entries {
            ret.max_table_entries = v;
        }
        ret
    }
}

#[cfg(feature = "memory-limiter")]
impl ResourceLimiter for MemoryLimit {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        max: Option<usize>,
    ) -> Result<bool, Error> {
        if max.map_or(false, |max| desired > max) {
            return Ok(false);
        } else if self.max_memory == u64::MAX {
            return Ok(true);
        }

        let delta = (desired - current) as u64;
        if let Some(v) = self.max_memory.checked_sub(delta) {
            self.max_memory = v;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn table_growing(
        &mut self,
        current: u32,
        desired: u32,
        max: Option<u32>,
    ) -> Result<bool, Error> {
        if max.map_or(false, |max| desired > max) {
            return Ok(false);
        } else if self.max_table_entries == u64::MAX {
            return Ok(true);
        }

        let delta = (desired - current) as u64;
        if let Some(v) = self.max_table_entries.checked_sub(delta) {
            self.max_table_entries = v;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl<T> InstanceData<T>
where
    T: AsRef<StoreData> + AsMut<StoreData>,
{
    pub fn instantiate(
        mut store: Store<T>,
        config: &Config,
        module: Gd<WasmModule>,
        host: Option<Dictionary>,
    ) -> Result<Self, Error> {
        config_store_common(&mut store, config)?;

        #[cfg(feature = "wasi")]
        let mut wasi_stdin = None;

        #[cfg(feature = "wasi")]
        let wasi_linker = if config.with_wasi {
            let mut builder = WasiCtxBuilder::new();

            let StoreData { wasi_ctx, .. } = store.data_mut().as_mut();

            if config.wasi_stdin == PipeBindingType::Instance {
                if let Some(data) = config.wasi_stdin_data.clone() {
                    builder.stdin(Box::new(ByteBufferReadPipe::new(data)));
                } else {
                    // TODO: Emit signal
                    let (outer, inner) = OuterStdin::new(move || {});
                    builder.stdin(Box::new(outer));
                    wasi_stdin = Some(inner as _);
                }
            }
            if config.wasi_stdout == PipeBindingType::Instance {
                builder.stdout(match config.wasi_stdout_buffer {
                    PipeBufferType::Unbuffered => {
                        Box::new(UnbufferedWritePipe::new(move |_buf| {})) as _
                    }
                    PipeBufferType::LineBuffer => Box::new(LineWritePipe::new(move |_buf| {})) as _,
                    PipeBufferType::BlockBuffer => {
                        Box::new(BlockWritePipe::new(move |_buf| {})) as _
                    }
                });
            }
            if config.wasi_stderr == PipeBindingType::Instance {
                builder.stderr(match config.wasi_stderr_buffer {
                    PipeBufferType::Unbuffered => {
                        Box::new(UnbufferedWritePipe::new(move |_buf| {})) as _
                    }
                    PipeBufferType::LineBuffer => Box::new(LineWritePipe::new(move |_buf| {})) as _,
                    PipeBufferType::BlockBuffer => {
                        Box::new(BlockWritePipe::new(move |_buf| {})) as _
                    }
                });
            }

            *wasi_ctx = match &config.wasi_context {
                Some(ctx) => {
                    MaybeWasi::Preview1(WasiContext::build_ctx(ctx.clone(), builder, config)?)
                }
                None => MaybeWasi::Preview1(WasiContext::init_ctx_no_context(
                    builder.inherit_stdout().inherit_stderr().build(),
                    config,
                )?),
            };
            let mut r = <Linker<T>>::new(&ENGINE);
            add_to_linker(&mut r, |data| match &mut data.as_mut().wasi_ctx {
                MaybeWasi::Preview1(ctx) => ctx,
                _ => panic!("Requested WASI Preview 1 interface while none set, this is a bug"),
            })?;
            Some(r)
        } else {
            None
        };

        #[cfg(feature = "object-registry-compat")]
        if config.extern_bind == ExternBindingType::Registry {
            store.data_mut().as_mut().object_registry = Some(ObjectRegistry::default());
        }

        let sp = &mut store;
        let instance = Self::instantiate_wasm(
            sp,
            config,
            module.bind().get_data()?,
            &mut HashMap::new(),
            &mut host.map(HostModuleCache::new),
            #[cfg(feature = "object-registry-compat")]
            &mut ObjregistryFuncs::default(),
            #[cfg(feature = "object-registry-extern")]
            &mut ExternrefFuncs::default(),
            #[cfg(feature = "wasi")]
            wasi_linker.as_ref(),
        )?;

        Ok(Self {
            instance: InstanceType::Core(instance),
            module,
            store: Mutex::new(store),
            #[cfg(feature = "wasi")]
            wasi_stdin,
        })
    }

    fn instantiate_wasm(
        store: &mut Store<T>,
        config: &Config,
        module: &ModuleData,
        insts: &mut HashMap<InstanceId, InstanceWasm>,
        host: &mut Option<HostModuleCache<T>>,
        #[cfg(feature = "object-registry-compat")] objregistry_funcs: &mut ObjregistryFuncs,
        #[cfg(feature = "object-registry-extern")] externref_funcs: &mut ExternrefFuncs,
        #[cfg(feature = "wasi")] wasi_linker: Option<&Linker<T>>,
    ) -> Result<InstanceWasm, Error> {
        #[allow(irrefutable_let_patterns)]
        let ModuleType::Core(module_) = &module.module
        else {
            bail_with_site!("Cannot instantiate component")
        };
        let it = module_.imports();
        let mut imports = Vec::with_capacity(it.len());

        for i in it {
            if let Some(v) = host
                .as_mut()
                .and_then(|v| v.get_extern(&mut *store, i.module(), i.name()).transpose())
                .transpose()?
            {
                imports.push(v);
                continue;
            }

            match (i.module(), config) {
                #[cfg(feature = "object-registry-compat")]
                (
                    OBJREGISTRY_MODULE,
                    Config {
                        extern_bind: ExternBindingType::Registry,
                        ..
                    },
                ) => {
                    if let Some(v) =
                        objregistry_funcs.get_func(&mut store.as_context_mut(), i.name())
                    {
                        imports.push(v.into());
                        continue;
                    }
                }
                #[cfg(feature = "object-registry-extern")]
                (
                    EXTERNREF_MODULE,
                    Config {
                        extern_bind: ExternBindingType::Native,
                        ..
                    },
                ) => {
                    if let Some(v) = externref_funcs.get_func(&mut store.as_context_mut(), i.name())
                    {
                        imports.push(v.into());
                        continue;
                    }
                }
                _ => (),
            }

            #[cfg(feature = "wasi")]
            if let Some(l) = wasi_linker.as_ref() {
                if let Some(v) = l.get_by_import(&mut *store, &i) {
                    imports.push(v);
                    continue;
                }
            }

            if let Some(v) = module.imports.get(i.module()) {
                let v = loop {
                    match insts.get(&v.instance_id()) {
                        Some(v) => break v,
                        None => {
                            let t = Self::instantiate_wasm(
                                &mut *store,
                                config,
                                v.bind().get_data()?,
                                &mut *insts,
                                &mut *host,
                                #[cfg(feature = "object-registry-compat")]
                                &mut *objregistry_funcs,
                                #[cfg(feature = "object-registry-extern")]
                                &mut *externref_funcs,
                                #[cfg(feature = "wasi")]
                                wasi_linker,
                            )?;
                            insts.insert(v.instance_id(), t);
                        }
                    }
                };

                if let Some(v) = v.get_export(&mut *store, i.name()) {
                    imports.push(v.clone());
                    continue;
                }
            }

            bail_with_site!("Unknown import {:?}.{:?}", i.module(), i.name());
        }

        InstanceWasm::new(store, module_, &imports)
    }
}

impl<T> InstanceData<T>
where
    T: AsRef<InnerLock> + AsMut<InnerLock>,
{
    pub fn acquire_store<F, R>(&self, f: F) -> R
    where
        for<'a> F: FnOnce(&Self, StoreContextMut<'a, T>) -> R,
    {
        let mut guard_ = self.store.lock();

        let _scope;
        // SAFETY: Context should be destroyed after function call
        unsafe {
            let p = &mut guard_.data_mut().as_mut().mutex_raw as *mut _;
            let mut v = self.store.raw() as *const _;
            ptr::swap(p, &mut v);
            _scope = guard(p, move |p| {
                *p = v;
            });
        }

        f(self, guard_.as_context_mut())
    }
}

impl InnerLock {
    pub fn release_store<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard;
        if !self.mutex_raw.is_null() {
            // SAFETY: Pointer is valid and locked mutex
            unsafe {
                _guard = guard(&*self.mutex_raw, |v| v.lock());
                _guard.unlock();
            }
        }

        f()
    }
}

impl StoreData {
    #[inline]
    pub(crate) fn release_store<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.inner_lock.release_store(f)
    }

    #[cfg(feature = "object-registry-compat")]
    pub fn get_registry(&self) -> Result<&ObjectRegistry, Error> {
        site_context!(self
            .object_registry
            .as_ref()
            .ok_or_else(|| Error::msg("Object registry not enabled!")))
    }

    #[cfg(feature = "object-registry-compat")]
    pub fn get_registry_mut(&mut self) -> Result<&mut ObjectRegistry, Error> {
        site_context!(self
            .object_registry
            .as_mut()
            .ok_or_else(|| Error::msg("Object registry not enabled!")))
    }
}

impl WasmInstance {
    pub fn get_data(&self) -> Result<&InstanceData<StoreData>, Error> {
        if let Some(data) = self.data.get() {
            Ok(data)
        } else {
            bail_with_site!("Uninitialized instance")
        }
    }

    pub fn unwrap_data<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&InstanceData<StoreData>) -> Result<R, Error>,
    {
        match self.get_data().and_then(f) {
            Ok(v) => Some(v),
            Err(e) => {
                /*
                error(
                    e.downcast_ref::<Site>()
                        .copied()
                        .unwrap_or_else(|| godot_site!()),
                    &s,
                );
                */
                godot_error!("{:?}", e);
                /*
                self.base.emit_signal(
                    StringName::from("error_happened"),
                    &[format!("{}", e).to_variant()],
                );
                */
                None
            }
        }
    }

    pub fn initialize_(
        &self,
        module: Gd<WasmModule>,
        host: Option<Dictionary>,
        config: Option<Variant>,
    ) -> bool {
        let r = self.data.get_or_try_init(move || -> Result<_, Error> {
            let mut ret = InstanceData::instantiate(
                Store::new(&ENGINE, StoreData::default()),
                &match config {
                    Some(v) => match Config::try_from_variant(&v) {
                        Ok(v) => v,
                        Err(e) => {
                            godot_error!("{:?}", e);
                            Config::default()
                        }
                    },
                    None => Config::default(),
                },
                module,
                host,
            )?;

            // SAFETY: Nobody else can access memory
            #[allow(mutable_transmutes)]
            unsafe {
                *mem::transmute::<_, &mut Option<Memory>>(&self.memory) = match &ret.instance {
                    InstanceType::Core(inst) => inst.get_memory(ret.store.get_mut(), MEMORY_EXPORT),
                    #[allow(unreachable_patterns)]
                    _ => None,
                };
            }
            Ok(ret)
        });
        if let Err(e) = r {
            godot_error!("{:?}", e);
            false
        } else {
            true
        }
    }

    fn get_memory<F, R>(&self, f: F) -> Option<R>
    where
        for<'a> F: FnOnce(StoreContextMut<'a, StoreData>, Memory) -> Result<R, Error>,
    {
        self.unwrap_data(|m| {
            m.acquire_store(|_, store| match self.memory {
                Some(mem) => f(store, mem),
                None => bail_with_site!("No memory exported"),
            })
        })
    }

    fn read_memory<F, R>(&self, i: usize, n: usize, f: F) -> Option<R>
    where
        F: FnOnce(&[u8]) -> Result<R, Error>,
    {
        self.get_memory(|store, mem| {
            let data = mem.data(&store);
            match data.get(i..i + n) {
                Some(s) => f(s),
                None => bail_with_site!("Index out of bound {}-{}", i, i + n),
            }
        })
    }

    fn write_memory<F, R>(&self, i: usize, n: usize, f: F) -> Option<R>
    where
        for<'a> F: FnOnce(&'a mut [u8]) -> Result<R, Error>,
    {
        self.get_memory(|mut store, mem| {
            let data = mem.data_mut(&mut store);
            match data.get_mut(i..i + n) {
                Some(s) => f(s),
                None => bail_with_site!("Index out of bound {}-{}", i, i + n),
            }
        })
    }
}

struct WasmCallable {
    name: StringName,
    ty: FuncType,
    f: Func,
    this: SendSyncWrapper<Gd<WasmInstance>>,
}

impl PartialEq for WasmCallable {
    fn eq(&self, other: &Self) -> bool {
        (self.name == other.name) && (*self.this == *other.this) && (self.ty == other.ty)
    }
}

impl Eq for WasmCallable {}

impl Hash for WasmCallable {
    fn hash<H: Hasher>(&self, state: &mut H) {
        <StringName as Hash>::hash(&self.name, state);
        self.ty.hash(state);
        self.this.hash(state);
    }
}

impl fmt::Debug for WasmCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WasmCallable {{ object: {:?}, name: {:?}, type: {:?}, func: {:?} }}",
            *self.this, self.name, self.ty, self.f,
        )
    }
}

impl fmt::Display for WasmCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WasmCallable({:?}.{}<(", *self.this, self.name)?;

        let mut start = true;
        for v in self.ty.params() {
            let s = if start {
                start = false;
                ", "
            } else {
                ""
            };
            write!(f, "{s}{v}")?;
        }

        write!(f, "), (")?;
        start = false;
        for v in self.ty.results() {
            let s = if start {
                start = false;
                ", "
            } else {
                ""
            };
            write!(f, "{s}{v}")?;
        }

        write!(f, ")>)")
    }
}

impl RustCallable for WasmCallable {
    fn invoke(&mut self, args: &[&Variant]) -> Result<Variant, ()> {
        let ty = &self.ty;
        let f = &self.f;
        let f = move |_: &'_ _, mut store: StoreContextMut<'_, StoreData>| {
            let pi = ty.params();
            let ri = ty.results();
            let mut arr = Vec::with_capacity(pi.len().max(ri.len()));

            store.gc();

            let pl = pi.len();
            for (t, v) in pi.zip(args) {
                arr.push(unsafe { to_raw(&mut store, t, (**v).clone())? });
            }
            if args.len() != pl {
                bail_with_site!("Too few parameter (expected {}, got {})", pl, args.len());
            }
            while arr.len() < ri.len() {
                arr.push(ValRaw::i32(0));
            }

            #[cfg(feature = "epoch-timeout")]
            if let v @ 1.. = store.data().epoch_timeout {
                store.set_epoch_deadline(v);
            }

            // SAFETY: Array length is maximum of params and returns and initialized
            unsafe {
                site_context!(f.call_unchecked(&mut store, arr.as_mut_ptr(), arr.len()))?;
            }

            let mut ret = Array::new();
            for (t, v) in ri.zip(arr) {
                ret.push(unsafe { from_raw(&mut store, t, v)? });
            }

            Ok(ret.to_variant())
        };

        self.this
            .bind()
            .unwrap_data(move |m| m.acquire_store(f))
            .ok_or(())
    }
}

#[godot_api]
impl WasmInstance {
    #[signal]
    fn error_happened();

    /// Initialize and loads module.
    /// MUST be called for the first time and only once.
    #[func]
    fn initialize(
        &self,
        module: Gd<WasmModule>,
        host: Variant,
        config: Variant,
    ) -> Option<Gd<WasmInstance>> {
        let Ok(host) = variant_to_option::<Dictionary>(host) else {
            godot_error!("Host is not a dictionary!");
            return None;
        };
        let config = if config.is_nil() { None } else { Some(config) };

        if self.initialize_(module, host, config) {
            Some(self.to_gd())
        } else {
            None
        }
    }

    #[func]
    fn get_module(&self) -> Option<Gd<WasmModule>> {
        self.unwrap_data(|m| Ok(m.module.clone()))
    }

    #[func]
    fn call_wasm(&self, name: StringName, args: Array<Variant>) -> Array<Variant> {
        self.unwrap_data(move |m| {
            m.acquire_store(move |m, mut store| {
                let name = name.to_string();
                let f = match site_context!(m.instance.get_core())?.get_export(&mut store, &name) {
                    Some(f) => match f {
                        Extern::Func(f) => f,
                        _ => bail_with_site!("Export {} is not a function", &name),
                    },
                    None => bail_with_site!("Export {} does not exists", &name),
                };

                store.gc();

                let ty = f.ty(&store);
                let pi = ty.params();
                let ri = ty.results();
                let mut arr = Vec::with_capacity(pi.len().max(ri.len()));

                let pl = pi.len();
                for (t, v) in pi.zip(args.iter_shared()) {
                    arr.push(unsafe { to_raw(&mut store, t, v)? });
                }
                if arr.len() != pl {
                    bail_with_site!("Too few parameter (expected {}, got {})", pl, arr.len());
                }
                while arr.len() < ri.len() {
                    arr.push(ValRaw::i32(0));
                }

                #[cfg(feature = "epoch-timeout")]
                if let v @ 1.. = store.data().epoch_timeout {
                    store.set_epoch_deadline(v);
                }

                // SAFETY: Array length is maximum of params and returns and initialized
                unsafe {
                    site_context!(f.call_unchecked(&mut store, arr.as_mut_ptr(), arr.len()))?;
                }

                let mut ret = Array::new();
                for (t, v) in ri.zip(arr) {
                    ret.push(unsafe { from_raw(&mut store, t, v)? });
                }

                Ok(ret)
            })
        })
        .unwrap_or_default()
    }

    #[func]
    fn bind_wasm_callable(&self, name: StringName) -> Callable {
        self.unwrap_data(|m| {
            m.acquire_store(|m, mut store| {
                let n = name.to_string();
                let f = match site_context!(m.instance.get_core())?.get_export(&mut store, &n) {
                    Some(f) => match f {
                        Extern::Func(f) => f,
                        _ => bail_with_site!("Export {} is not a function", &n),
                    },
                    None => bail_with_site!("Export {} does not exists", &n),
                };
                let ty = f.ty(&store);

                let this = SendSyncWrapper::new(self.to_gd());
                Ok(Callable::from_custom(WasmCallable { name, ty, f, this }))
            })
        })
        .unwrap_or_else(Callable::invalid)
    }

    /// Emit trap when returning from host. Only used for host binding.
    /// Returns previous error message, if any.
    #[func]
    fn signal_error(&self, msg: GString) -> Variant {
        option_to_variant(
            self.unwrap_data(|m| {
                m.acquire_store(|_, mut store| {
                    Ok(store.data_mut().error_signal.replace(msg.to_string()))
                })
            })
            .flatten(),
        )
    }

    /// Cancel effect of signal_error.
    /// Returns previous error message, if any.
    #[func]
    fn signal_error_cancel(&self) -> Variant {
        option_to_variant(
            self.unwrap_data(|m| {
                m.acquire_store(|_, mut store| Ok(store.data_mut().error_signal.take()))
            })
            .flatten(),
        )
    }

    #[func]
    fn reset_epoch(&self) {
        cfg_if! {
            if #[cfg(feature = "epoch-timeout")] {
                self.unwrap_data(|m| {
                    m.acquire_store(|_, mut store| {
                        if let v @ 1.. = store.data().epoch_timeout {
                            store.set_epoch_deadline(v);
                        }
                        Ok(())
                    })
                });
            } else {
                godot_error!("Feature epoch-timeout not enabled!");
            }
        }
    }

    #[func]
    fn register_object(&self, _obj: Variant) -> Variant {
        cfg_if! {
            if #[cfg(feature = "object-registry-compat")] {
                option_to_variant(self.unwrap_data(|m| {
                    if _obj.is_nil() {
                        bail_with_site!("Value is null!");
                    }
                    m.acquire_store(|_, mut store| Ok(store.data_mut().get_registry_mut()?.register(_obj) as u64))
                }))
            } else {
                godot_error!("Feature object-registry-compat not enabled!");
                Variant::nil()
            }
        }
    }

    #[func]
    fn registry_get(&self, _ix: i64) -> Variant {
        cfg_if! {
            if #[cfg(feature = "object-registry-compat")] {
                option_to_variant(
                    self.unwrap_data(|m| {
                        m.acquire_store(|_, store| {
                            Ok(store.data().get_registry()?.get(usize::try_from(_ix)?))
                        })
                    })
                    .flatten(),
                )
            } else {
                godot_error!("Feature object-registry-compat not enabled!");
                Variant::nil()
            }
        }
    }

    #[func]
    fn registry_set(&self, _ix: i64, _obj: Variant) -> Variant {
        cfg_if! {
            if #[cfg(feature = "object-registry-compat")] {
                option_to_variant(
                    self.unwrap_data(|m| {
                        m.acquire_store(|_, mut store| {
                            let _ix = usize::try_from(_ix)?;
                            let reg = store.data_mut().get_registry_mut()?;
                            if _obj.is_nil() {
                                Ok(reg.unregister(_ix))
                            } else {
                                Ok(reg.replace(_ix, _obj))
                            }
                        })
                    })
                    .flatten(),
                )
            } else {
                godot_error!("Feature object-registry-compat not enabled!");
                Variant::nil()
            }
        }
    }

    #[func]
    fn unregister_object(&self, _ix: i64) -> Variant {
        cfg_if! {
            if #[cfg(feature = "object-registry-compat")] {
                option_to_variant(
                    self.unwrap_data(|m| {
                        m.acquire_store(|_, mut store| {
                            Ok(store
                                .data_mut()
                                .get_registry_mut()?
                                .unregister(usize::try_from(_ix)?))
                        })
                    })
                    .flatten(),
                )
            } else {
                godot_error!("Feature object-registry-compat not enabled!");
                Variant::nil()
            }
        }
    }

    #[func]
    fn has_memory(&self) -> bool {
        self.unwrap_data(|m| m.acquire_store(|_, _| Ok(self.memory.is_some())))
            .unwrap_or_default()
    }

    #[func]
    fn memory_set_name(&self, name: GString) -> bool {
        self.unwrap_data(|m| {
            m.acquire_store(|m, store| {
                // SAFETY: Nobody else can access memory
                #[allow(mutable_transmutes)]
                unsafe {
                    *mem::transmute::<_, &mut Option<Memory>>(&self.memory) = match &m.instance {
                        InstanceType::Core(inst) => inst.get_memory(store, &name.to_string()),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    };
                }
                Ok(self.memory.is_some())
            })
        })
        .unwrap_or_default()
    }

    #[func]
    fn stdin_add_line(&self, line: GString) {
        cfg_if! {
            if #[cfg(feature = "wasi")] {
                self.unwrap_data(|m| {
                    if let Some(stdin) = &m.wasi_stdin {
                        stdin.add_line(line)?;
                    }
                    Ok(())
                });
            } else {
                godot_error!("Feature wasi not enabled!");
            }
        }
    }

    #[func]
    fn stdin_close(&self) {
        cfg_if! {
            if #[cfg(feature = "wasi")] {
                self.unwrap_data(|m| {
                    if let Some(stdin) = &m.wasi_stdin {
                        stdin.close_pipe();
                    }
                    Ok(())
                });
            } else {
                godot_error!("Feature wasi not enabled!");
            }
        }
    }

    #[func]
    fn memory_size(&self) -> i64 {
        self.get_memory(|store, mem| Ok(mem.data_size(store) as i64))
            .unwrap_or_default()
    }

    #[func]
    fn memory_read(&self, i: i64, n: i64) -> PackedByteArray {
        self.read_memory(i as _, n as _, |s| Ok(PackedByteArray::from(s)))
            .unwrap_or_default()
    }

    #[func]
    fn memory_write(&self, i: i64, a: PackedByteArray) -> bool {
        let a = a.to_vec();
        self.write_memory(i as _, a.len(), |s| {
            s.copy_from_slice(&a);
            Ok(())
        })
        .is_some()
    }

    #[func]
    fn get_8(&self, i: i64) -> i64 {
        self.read_memory(i as _, 1, |s| Ok(s[0]))
            .unwrap_or_default()
            .into()
    }

    #[func]
    fn put_8(&self, i: i64, v: i64) -> bool {
        self.write_memory(i as _, 1, |s| {
            s[0] = (v & 255) as _;
            Ok(())
        })
        .is_some()
    }

    #[func]
    fn get_16(&self, i: i64) -> i64 {
        self.read_memory(i as _, 2, |s| Ok(u16::from_le_bytes(s.try_into().unwrap())))
            .unwrap_or_default()
            .into()
    }

    #[func]
    fn put_16(&self, i: i64, v: i64) -> bool {
        self.write_memory(i as _, 2, |s| {
            s.copy_from_slice(&((v & 0xffff) as u16).to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[func]
    fn get_32(&self, i: i64) -> i64 {
        self.read_memory(i as _, 4, |s| Ok(u32::from_le_bytes(s.try_into().unwrap())))
            .unwrap_or_default()
            .into()
    }

    #[func]
    fn put_32(&self, i: i64, v: i64) -> bool {
        self.write_memory(i as _, 4, |s| {
            s.copy_from_slice(&((v & 0xffffffff) as u32).to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[func]
    fn get_64(&self, i: i64) -> i64 {
        self.read_memory(i as _, 8, |s| Ok(i64::from_le_bytes(s.try_into().unwrap())))
            .unwrap_or_default()
    }

    #[func]
    fn put_64(&self, i: i64, v: i64) -> bool {
        self.write_memory(i as _, 8, |s| {
            s.copy_from_slice(&v.to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[func]
    fn get_float(&self, i: i64) -> f64 {
        self.read_memory(i as _, 4, |s| Ok(f32::from_le_bytes(s.try_into().unwrap())))
            .unwrap_or_default()
            .into()
    }

    #[func]
    fn put_float(&self, i: i64, v: f64) -> bool {
        self.write_memory(i as _, 4, |s| {
            s.copy_from_slice(&(v as f32).to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[func]
    fn get_double(&self, i: i64) -> f64 {
        self.read_memory(i as _, 8, |s| Ok(f64::from_le_bytes(s.try_into().unwrap())))
            .unwrap_or_default()
    }

    #[func]
    fn put_double(&self, i: i64, v: f64) -> bool {
        self.write_memory(i as _, 8, |s| {
            s.copy_from_slice(&v.to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[func]
    fn put_array(&self, i: i64, v: Variant) -> bool {
        fn f<const N: usize, T: Sync>(
            d: &mut [u8],
            i: usize,
            s: &[T],
            f: impl Fn(&T, &mut [u8; N]) + Send + Sync,
        ) -> Result<(), Error> {
            let e = i + s.len() * N;
            let Some(d) = d.get_mut(i..e) else {
                bail_with_site!("Index out of range ({}..{})", i, e);
            };

            s.par_iter()
                .zip(d.par_chunks_exact_mut(N))
                .for_each(|(s, d)| f(s, d.try_into().unwrap()));

            Ok(())
        }

        self.get_memory(|mut store, mem| {
            let i = i as usize;
            let data = mem.data_mut(&mut store);
            match VariantDispatch::from(&v) {
                VariantDispatch::PackedByteArray(v) => {
                    let s = v.as_slice();
                    let e = i + s.len();
                    let Some(d) = data.get_mut(i..e) else {
                        bail_with_site!("Index out of range ({}..{})", i, e);
                    };

                    d.copy_from_slice(s);
                    Ok(())
                }
                VariantDispatch::PackedInt32Array(v) => {
                    f::<4, _>(data, i, v.as_slice(), |s, d| *d = s.to_le_bytes())
                }
                VariantDispatch::PackedInt64Array(v) => {
                    f::<8, _>(data, i, v.as_slice(), |s, d| *d = s.to_le_bytes())
                }
                VariantDispatch::PackedFloat32Array(v) => {
                    f::<4, _>(data, i, v.as_slice(), |s, d| *d = s.to_le_bytes())
                }
                VariantDispatch::PackedFloat64Array(v) => {
                    f::<8, _>(data, i, v.as_slice(), |s, d| *d = s.to_le_bytes())
                }
                VariantDispatch::PackedVector2Array(v) => {
                    f::<8, _>(data, i, v.as_slice(), |s, d| {
                        *<&mut [u8; 4]>::try_from(&mut d[..4]).unwrap() = s.x.to_le_bytes();
                        *<&mut [u8; 4]>::try_from(&mut d[4..]).unwrap() = s.y.to_le_bytes();
                    })
                }
                VariantDispatch::PackedVector3Array(v) => {
                    f::<12, _>(data, i, v.as_slice(), |s, d| {
                        *<&mut [u8; 4]>::try_from(&mut d[..4]).unwrap() = s.x.to_le_bytes();
                        *<&mut [u8; 4]>::try_from(&mut d[4..8]).unwrap() = s.y.to_le_bytes();
                        *<&mut [u8; 4]>::try_from(&mut d[8..]).unwrap() = s.z.to_le_bytes();
                    })
                }
                VariantDispatch::PackedColorArray(v) => {
                    f::<16, _>(data, i, v.as_slice(), |s, d| {
                        *<&mut [u8; 4]>::try_from(&mut d[..4]).unwrap() = s.r.to_le_bytes();
                        *<&mut [u8; 4]>::try_from(&mut d[4..8]).unwrap() = s.g.to_le_bytes();
                        *<&mut [u8; 4]>::try_from(&mut d[8..12]).unwrap() = s.b.to_le_bytes();
                        *<&mut [u8; 4]>::try_from(&mut d[12..]).unwrap() = s.a.to_le_bytes();
                    })
                }
                _ => bail_with_site!("Unknown value type {:?}", v.get_type()),
            }
        })
        .is_some()
    }

    #[func]
    fn get_array(&self, i: i64, n: i64, t: i64) -> Variant {
        fn f<const N: usize, T: Send, R: for<'a> From<&'a [T]>>(
            s: &[u8],
            i: usize,
            n: usize,
            f: impl Fn(&[u8; N]) -> T + Send + Sync,
        ) -> Result<R, Error> {
            let e = i + n * N;
            let Some(s) = s.get(i..e) else {
                bail_with_site!("Index out of range ({}..{})", i, e);
            };

            Ok(R::from(
                &s.par_chunks_exact(N)
                    .map(|s| f(s.try_into().unwrap()))
                    .collect::<Vec<_>>(),
            ))
        }

        option_to_variant(self.get_memory(|store, mem| {
            let (i, n) = (i as usize, n as usize);
            let data = mem.data(&store);
            match t {
                29 => {
                    let e = i + n;
                    let Some(s) = data.get(i..e) else {
                        bail_with_site!("Index out of range ({}..{})", i, e);
                    };

                    Ok(Variant::from(PackedByteArray::from(s)))
                }
                30 => Ok(Variant::from(f::<4, _, PackedInt32Array>(
                    data,
                    i,
                    n,
                    |s| i32::from_le_bytes(*s),
                )?)),
                31 => Ok(Variant::from(f::<8, _, PackedInt64Array>(
                    data,
                    i,
                    n,
                    |s| i64::from_le_bytes(*s),
                )?)),
                32 => Ok(Variant::from(f::<4, _, PackedFloat32Array>(
                    data,
                    i,
                    n,
                    |s| f32::from_le_bytes(*s),
                )?)),
                33 => Ok(Variant::from(f::<8, _, PackedFloat64Array>(
                    data,
                    i,
                    n,
                    |s| f64::from_le_bytes(*s),
                )?)),
                35 => Ok(Variant::from(f::<8, _, PackedVector2Array>(
                    data,
                    i,
                    n,
                    |s| Vector2 {
                        x: f32::from_le_bytes(s[..4].try_into().unwrap()),
                        y: f32::from_le_bytes(s[4..].try_into().unwrap()),
                    },
                )?)),
                36 => Ok(Variant::from(f::<12, _, PackedVector3Array>(
                    data,
                    i,
                    n,
                    |s| Vector3 {
                        x: f32::from_le_bytes(s[..4].try_into().unwrap()),
                        y: f32::from_le_bytes(s[4..8].try_into().unwrap()),
                        z: f32::from_le_bytes(s[8..].try_into().unwrap()),
                    },
                )?)),
                37 => Ok(Variant::from(f::<16, _, PackedColorArray>(
                    data,
                    i,
                    n,
                    |s| Color {
                        r: f32::from_le_bytes(s[..4].try_into().unwrap()),
                        g: f32::from_le_bytes(s[4..8].try_into().unwrap()),
                        b: f32::from_le_bytes(s[8..12].try_into().unwrap()),
                        a: f32::from_le_bytes(s[12..].try_into().unwrap()),
                    },
                )?)),
                ..=37 => bail_with_site!("Unsupported type ID {}", t),
                _ => bail_with_site!("Unknown type {}", t),
            }
        }))
    }

    #[func]
    fn read_struct(&self, format: GString, p: i64) -> Variant {
        option_to_variant(
            self.get_memory(|store, mem| read_struct(mem.data(store), p as _, &format.to_string())),
        )
    }

    #[func]
    fn write_struct(&self, format: GString, p: i64, arr: Array<Variant>) -> i64 {
        self.get_memory(|store, mem| {
            write_struct(mem.data_mut(store), p as _, &format.to_string(), arr)
        })
        .unwrap_or_default() as _
    }
}
