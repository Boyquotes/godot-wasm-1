use std::collections::HashMap;
use std::mem::{size_of, transmute};
use std::ptr;

use anyhow::{bail, Error};
use gdnative::core_types::PoolElement;
use gdnative::export::user_data::Map;
use gdnative::log::{error, godot_site, Site};
use gdnative::prelude::*;
use parking_lot::{lock_api::RawMutex as RawMutexTrait, Mutex, Once, OnceState, RawMutex};
use scopeguard::guard;
use wasmtime::{
    AsContextMut, Extern, Instance as InstanceWasm, Memory, Store, StoreContextMut, ValRaw,
};

use crate::wasm_config::{Config, ExternBindingType};
#[cfg(feature = "epoch-timeout")]
use crate::wasm_engine::EPOCH;
use crate::wasm_engine::{ModuleData, WasmModule, ENGINE};
#[cfg(feature = "object-registry-extern")]
use crate::wasm_externref::EXTERNREF_LINKER;
#[cfg(feature = "object-registry-compat")]
use crate::wasm_objregistry::{ObjectRegistry, OBJREGISTRY_LINKER};
#[cfg(feature = "epoch-timeout")]
use crate::wasm_util::EPOCH_DEADLINE;
#[cfg(feature = "object-registry-extern")]
use crate::wasm_util::EXTERNREF_MODULE;
#[cfg(feature = "object-registry-compat")]
use crate::wasm_util::OBJREGISTRY_MODULE;
use crate::wasm_util::{from_raw, make_host_module, to_raw, HOST_MODULE, MEMORY_EXPORT};
use crate::{bail_with_site, site_context};

#[derive(NativeClass)]
#[inherit(Reference)]
#[register_with(Self::register_properties)]
#[user_data(gdnative::export::user_data::ArcData<WasmInstance>)]
pub struct WasmInstance {
    once: Once,
    data: Option<InstanceData>,
}

pub struct InstanceData {
    store: Mutex<Store<StoreData>>,
    instance: InstanceWasm,
    module: Instance<WasmModule, Shared>,
}

pub struct StoreData {
    mutex_raw: *const RawMutex,
    pub config: Config,
    pub error_signal: Option<String>,
    #[cfg(feature = "object-registry-compat")]
    pub object_registry: Option<ObjectRegistry>,
}

// SAFETY: Store data is safely contained within instance data?
unsafe impl Send for StoreData {}
unsafe impl Sync for StoreData {}

impl InstanceData {
    pub fn instantiate(
        mut store: Store<StoreData>,
        module: Instance<WasmModule, Shared>,
        host: Option<Dictionary>,
    ) -> Result<Self, Error> {
        let config = store.data().config;

        #[cfg(feature = "epoch-timeout")]
        if config.with_epoch {
            store.epoch_deadline_trap();
            EPOCH.spawn_thread(|| ENGINE.increment_epoch());
        } else {
            store.epoch_deadline_callback(|_| Ok(EPOCH_DEADLINE));
        }

        match config.extern_bind {
            ExternBindingType::None => (),
            #[cfg(feature = "object-registry-compat")]
            ExternBindingType::Registry => {
                store.data_mut().object_registry = Some(ObjectRegistry::default());
            }
            _ => panic!("Unimplemented binding"),
        }

        type InstMap = HashMap<Ref<Reference, Shared>, InstanceWasm>;

        fn f(
            store: &mut Store<StoreData>,
            module: &ModuleData,
            insts: &mut InstMap,
            host: &Option<HashMap<String, Extern>>,
        ) -> Result<InstanceWasm, Error> {
            let it = module.module.imports();
            let mut imports = Vec::with_capacity(it.len());

            for i in it {
                match (i.module(), store.data().config) {
                    (HOST_MODULE, _) => {
                        if let Some(host) = host.as_ref() {
                            if let Some(v) = host.get(i.name()) {
                                imports.push(v.clone());
                                continue;
                            }
                        }
                    }
                    #[cfg(feature = "object-registry-compat")]
                    (
                        OBJREGISTRY_MODULE,
                        Config {
                            extern_bind: ExternBindingType::Registry,
                            ..
                        },
                    ) => {
                        if let Some(v) = OBJREGISTRY_LINKER.get_by_import(&mut *store, &i) {
                            imports.push(v);
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
                        if let Some(v) = EXTERNREF_LINKER.get_by_import(&mut *store, &i) {
                            imports.push(v);
                            continue;
                        }
                    }
                    _ => (),
                }

                if let Some(v) = module.imports.get(i.module()) {
                    let v = loop {
                        match insts.get(v.base()) {
                            Some(v) => break v,
                            None => {
                                let t = v
                                    .script()
                                    .map(|m| f(&mut *store, m.get_data()?, &mut *insts, host))
                                    .unwrap()?;
                                insts.insert(v.base().clone(), t);
                            }
                        }
                    };

                    if let Some(v) = v.get_export(&mut *store, i.name()) {
                        imports.push(v.clone());
                        continue;
                    }
                }

                bail!("Unknown import {:?}.{:?}", i.module(), i.name());
            }

            #[cfg(feature = "epoch-timeout")]
            store.set_epoch_deadline(store.data().config.epoch_timeout);
            InstanceWasm::new(store, &module.module, &imports)
        }

        let host = host.map(|h| make_host_module(&mut store, h)).transpose()?;

        let sp = &mut store;
        let instance = module
            .script()
            .map(move |m| {
                let mut insts = HashMap::new();
                f(sp, m.get_data()?, &mut insts, &host)
            })
            .unwrap()?;

        Ok(Self {
            instance,
            module,
            store: Mutex::new(store),
        })
    }

    fn acquire_store<F, R>(&self, f: F) -> R
    where
        for<'a> F: FnOnce(&Self, StoreContextMut<'a, StoreData>) -> R,
    {
        let mut guard_ = self.store.lock();

        let _scope;
        // SAFETY: Context should be destroyed after function call
        unsafe {
            let p = &mut guard_.data_mut().mutex_raw as *mut _;
            let mut v = self.store.raw() as *const _;
            ptr::swap(p, &mut v);
            _scope = guard(p, move |p| {
                *p = v;
            });
        }

        f(self, guard_.as_context_mut())
    }
}

impl StoreData {
    pub(crate) fn release_store<F, R>(&mut self, f: F) -> R
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
    fn new(_owner: &Reference) -> Self {
        Self {
            once: Once::new(),
            data: None,
        }
    }

    pub fn get_data(&self) -> Result<&InstanceData, Error> {
        if let OnceState::Done = self.once.state() {
            Ok(self.data.as_ref().unwrap())
        } else {
            bail_with_site!("Uninitialized module")
        }
    }

    pub fn unwrap_data<F, R>(&self, base: TRef<Reference>, f: F) -> Option<R>
    where
        F: FnOnce(&InstanceData) -> Result<R, Error>,
    {
        match self.get_data().and_then(f) {
            Ok(v) => Some(v),
            Err(e) => {
                let s = format!("{:?}", e);
                error(
                    e.downcast_ref::<Site>()
                        .copied()
                        .unwrap_or_else(|| godot_site!()),
                    &s,
                );
                base.emit_signal("error_happened", &[s.owned_to_variant()]);
                None
            }
        }
    }

    pub fn initialize_(
        &self,
        module: Instance<WasmModule, Shared>,
        host: Option<Dictionary>,
        config: Option<Variant>,
    ) -> bool {
        let mut r = true;
        let ret = &mut r;

        self.once.call_once(move || {
            match InstanceData::instantiate(
                Store::new(
                    &ENGINE,
                    StoreData {
                        mutex_raw: ptr::null(),
                        config: match config {
                            Some(v) => match Config::from_variant(&v) {
                                Ok(v) => v,
                                Err(e) => {
                                    godot_error!("{}", e);
                                    Config::default()
                                }
                            },
                            None => Config::default(),
                        },
                        error_signal: None,
                        #[cfg(feature = "object-registry-compat")]
                        object_registry: None,
                    },
                ),
                module,
                host,
            ) {
                Ok(v) => {
                    // SAFETY: Should be called only once and nobody else can read module data
                    #[allow(mutable_transmutes)]
                    let data = unsafe {
                        transmute::<&Option<InstanceData>, &mut Option<InstanceData>>(&self.data)
                    };
                    *data = Some(v);
                }
                Err(e) => {
                    godot_error!("{}", e);
                    *ret = false;
                }
            }
        });

        r
    }

    fn get_memory<F, R>(&self, base: TRef<Reference>, f: F) -> Option<R>
    where
        for<'a> F: FnOnce(StoreContextMut<'a, StoreData>, Memory) -> Result<R, Error>,
    {
        self.unwrap_data(base, |m| {
            m.acquire_store(
                |m, mut store| match m.instance.get_memory(&mut store, MEMORY_EXPORT) {
                    Some(mem) => f(store, mem),
                    None => bail_with_site!("No memory exported"),
                },
            )
        })
    }

    fn read_memory<F, R>(&self, base: TRef<Reference>, i: usize, n: usize, f: F) -> Option<R>
    where
        F: FnOnce(&[u8]) -> Result<R, Error>,
    {
        self.get_memory(base, |store, mem| {
            let data = mem.data(&store);
            match data.get(i..i + n) {
                Some(s) => f(s),
                None => bail_with_site!("Index out of bound {}-{}", i, i + n),
            }
        })
    }

    fn write_memory<F, R>(&self, base: TRef<Reference>, i: usize, n: usize, f: F) -> Option<R>
    where
        for<'a> F: FnOnce(&'a mut [u8]) -> Result<R, Error>,
    {
        self.get_memory(base, |mut store, mem| {
            let data = mem.data_mut(&mut store);
            match data.get_mut(i..i + n) {
                Some(s) => f(s),
                None => bail!("Index out of bound {}-{}", i, i + n),
            }
        })
    }
}

#[methods]
impl WasmInstance {
    /// Register properties
    fn register_properties(builder: &ClassBuilder<Self>) {
        builder
            .property::<Option<Instance<WasmModule, Shared>>>("module")
            .with_getter(|v, b| v.unwrap_data(b, |m| Ok(m.module.clone())))
            .done();

        builder
            .signal("error_happened")
            .with_param("message", VariantType::GodotString)
            .done();
    }

    /// Initialize and loads module.
    /// MUST be called for the first time and only once.
    #[method]
    fn initialize(
        &self,
        #[base] owner: TRef<Reference>,
        module: Instance<WasmModule, Shared>,
        #[opt] host: Option<Dictionary>,
        #[opt] config: Option<Variant>,
    ) -> Option<Ref<Reference>> {
        if self.initialize_(module, host, config) {
            Some(owner.claim())
        } else {
            None
        }
    }

    #[method]
    fn call_wasm(
        &self,
        #[base] base: TRef<Reference>,
        name: String,
        args: VariantArray,
    ) -> Option<VariantArray> {
        self.unwrap_data(base, move |m| {
            m.acquire_store(move |m, mut store| {
                let f = match m.instance.get_export(&mut store, &name) {
                    Some(f) => match f {
                        Extern::Func(f) => f,
                        _ => bail_with_site!("Export {} is not a function", &name),
                    },
                    None => bail_with_site!("Export {} does not exists", &name),
                };

                let ty = f.ty(&store);
                let pi = ty.params();
                let ri = ty.results();
                let mut arr = vec![ValRaw::i32(0); pi.len().max(ri.len())];

                for (ix, t) in pi.enumerate() {
                    arr[ix] = unsafe { to_raw(&mut store, t, args.get(ix as _))? };
                }

                #[cfg(feature = "epoch-timeout")]
                store.set_epoch_deadline(store.data().config.epoch_timeout);
                store.gc();
                // SAFETY: Array length is maximum of params and returns and initialized
                unsafe {
                    site_context!(f.call_unchecked(&mut store, arr.as_mut_ptr()))?;
                }

                let ret = VariantArray::new();
                for (ix, t) in ri.enumerate() {
                    ret.push(unsafe { from_raw(&mut store, t, arr[ix])? });
                }

                Ok(ret.into_shared())
            })
        })
    }

    /// Emit trap when returning from host. Only used for host binding.
    /// Returns previous error message, if any.
    #[method]
    fn signal_error(&self, #[base] base: TRef<Reference>, msg: String) -> Option<String> {
        self.unwrap_data(base, |m| {
            m.acquire_store(|_, mut store| Ok(store.data_mut().error_signal.replace(msg)))
        })
        .flatten()
    }

    /// Cancel effect of signal_error.
    /// Returns previous error message, if any.
    #[method]
    fn signal_error_cancel(&self, #[base] base: TRef<Reference>) -> Option<String> {
        self.unwrap_data(base, |m| {
            m.acquire_store(|_, mut store| Ok(store.data_mut().error_signal.take()))
        })
        .flatten()
    }

    #[method]
    fn reset_epoch(&self, #[base] _base: TRef<Reference>) {
        #[cfg(feature = "epoch-timeout")]
        self.unwrap_data(_base, |m| {
            m.acquire_store(|_, mut store| {
                store.set_epoch_deadline(store.data().config.epoch_timeout);
                Ok(())
            })
        });

        #[cfg(not(feature = "epoch-timeout"))]
        godot_error!("Feature epoch-timeout not enabled!");
    }

    #[method]
    fn register_object(&self, #[base] _base: TRef<Reference>, _obj: Variant) -> Option<usize> {
        #[cfg(feature = "object-registry-compat")]
        return self.unwrap_data(_base, |m| {
            if _obj.is_nil() {
                bail_with_site!("Value is null!");
            }
            m.acquire_store(|_, mut store| Ok(store.data_mut().get_registry_mut()?.register(_obj)))
        });

        #[cfg(not(feature = "object-registry-compat"))]
        {
            godot_error!("Feature object-registry-compat not enabled!");
            None
        }
    }

    #[method]
    fn registry_get(&self, #[base] _base: TRef<Reference>, _ix: usize) -> Option<Variant> {
        #[cfg(feature = "object-registry-compat")]
        return self
            .unwrap_data(_base, |m| {
                m.acquire_store(|_, store| Ok(store.data().get_registry()?.get(_ix)))
            })
            .flatten();

        #[cfg(not(feature = "object-registry-compat"))]
        {
            godot_error!("Feature object-registry-compat not enabled!");
            None
        }
    }

    #[method]
    fn registry_set(
        &self,
        #[base] _base: TRef<Reference>,
        _ix: usize,
        _obj: Variant,
    ) -> Option<Variant> {
        #[cfg(feature = "object-registry-compat")]
        return self
            .unwrap_data(_base, |m| {
                m.acquire_store(|_, mut store| {
                    let reg = store.data_mut().get_registry_mut()?;
                    if _obj.is_nil() {
                        Ok(reg.unregister(_ix))
                    } else {
                        Ok(reg.replace(_ix, _obj))
                    }
                })
            })
            .flatten();

        #[cfg(not(feature = "object-registry-compat"))]
        {
            godot_error!("Feature object-registry-compat not enabled!");
            None
        }
    }

    #[method]
    fn unregister_object(&self, #[base] _base: TRef<Reference>, _ix: usize) -> Option<Variant> {
        #[cfg(feature = "object-registry-compat")]
        return self
            .unwrap_data(_base, |m| {
                m.acquire_store(|_, mut store| {
                    Ok(store.data_mut().get_registry_mut()?.unregister(_ix))
                })
            })
            .flatten();

        #[cfg(not(feature = "object-registry-compat"))]
        {
            godot_error!("Feature object-registry-compat not enabled!");
            None
        }
    }

    #[method]
    fn has_memory(&self, #[base] base: TRef<Reference>) -> bool {
        self.unwrap_data(base, |m| {
            m.acquire_store(|m, mut store| {
                Ok(matches!(
                    m.instance.get_export(&mut store, MEMORY_EXPORT),
                    Some(Extern::Memory(_))
                ))
            })
        })
        .unwrap_or_default()
    }

    #[method]
    fn memory_size(&self, #[base] base: TRef<Reference>) -> usize {
        self.get_memory(base, |store, mem| Ok(mem.data_size(store)))
            .unwrap_or_default()
    }

    #[method]
    fn memory_read(
        &self,
        #[base] base: TRef<Reference>,
        i: usize,
        n: usize,
    ) -> Option<PoolArray<u8>> {
        self.read_memory(base, i, n, |s| Ok(<PoolArray<u8>>::from_slice(s)))
    }

    #[method]
    fn memory_write(&self, #[base] base: TRef<Reference>, i: usize, a: PoolArray<u8>) -> bool {
        let a = &*a.read();
        self.write_memory(base, i, a.len(), |s| {
            s.copy_from_slice(a);
            Ok(())
        })
        .is_some()
    }

    #[method]
    fn get_8(&self, #[base] base: TRef<Reference>, i: usize) -> Option<u8> {
        self.read_memory(base, i, 1, |s| Ok(s[0]))
    }

    #[method]
    fn put_8(&self, #[base] base: TRef<Reference>, i: usize, v: u8) -> bool {
        self.write_memory(base, i, 1, |s| {
            s[0] = v;
            Ok(())
        })
        .is_some()
    }

    #[method]
    fn get_16(&self, #[base] base: TRef<Reference>, i: usize) -> Option<u16> {
        self.read_memory(base, i, 2, |s| {
            Ok(u16::from_le_bytes(s.try_into().unwrap()))
        })
    }

    #[method]
    fn put_16(&self, #[base] base: TRef<Reference>, i: usize, v: u16) -> bool {
        self.write_memory(base, i, 2, |s| {
            s.copy_from_slice(&v.to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[method]
    fn get_32(&self, #[base] base: TRef<Reference>, i: usize) -> Option<u32> {
        self.read_memory(base, i, 4, |s| {
            Ok(u32::from_le_bytes(s.try_into().unwrap()))
        })
    }

    #[method]
    fn put_32(&self, #[base] base: TRef<Reference>, i: usize, v: u32) -> bool {
        self.write_memory(base, i, 4, |s| {
            s.copy_from_slice(&v.to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[method]
    fn get_64(&self, #[base] base: TRef<Reference>, i: usize) -> Option<i64> {
        self.read_memory(base, i, 8, |s| {
            Ok(i64::from_le_bytes(s.try_into().unwrap()))
        })
    }

    #[method]
    fn put_64(&self, #[base] base: TRef<Reference>, i: usize, v: i64) -> bool {
        self.write_memory(base, i, 8, |s| {
            s.copy_from_slice(&v.to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[method]
    fn get_float(&self, #[base] base: TRef<Reference>, i: usize) -> Option<f32> {
        self.read_memory(base, i, 4, |s| {
            Ok(f32::from_le_bytes(s.try_into().unwrap()))
        })
    }

    #[method]
    fn put_float(&self, #[base] base: TRef<Reference>, i: usize, v: f32) -> bool {
        self.write_memory(base, i, 4, |s| {
            s.copy_from_slice(&v.to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[method]
    fn get_double(&self, #[base] base: TRef<Reference>, i: usize) -> Option<f64> {
        self.read_memory(base, i, 8, |s| {
            Ok(f64::from_le_bytes(s.try_into().unwrap()))
        })
    }

    #[method]
    fn put_double(&self, #[base] base: TRef<Reference>, i: usize, v: f64) -> bool {
        self.write_memory(base, i, 8, |s| {
            s.copy_from_slice(&v.to_le_bytes());
            Ok(())
        })
        .is_some()
    }

    #[method]
    fn put_array(&self, #[base] base: TRef<Reference>, i: usize, v: Variant) -> bool {
        fn f<T: Copy>(d: &mut [u8], i: usize, s: &[T]) -> Result<(), Error> {
            let l = s.len() * size_of::<T>();
            let e = i + l;

            if let Some(d) = d.get_mut(i..e) {
                let ps = s.as_ptr() as *const u8;
                let pd = d.as_mut_ptr();

                // SAFETY: Source and destination is of the same size.
                // alignment of destination should be enforced externally.
                unsafe {
                    ptr::copy_nonoverlapping(ps, pd, l);
                }

                #[cfg(target_endian = "big")]
                if size_of::<T>() > 1 {
                    for d in d.chunks_mut(size_of::<T>()) {
                        debug_assert_eq!(d.len(), size_of::<T>());
                        d.reverse();
                    }
                }
            } else {
                bail_with_site!("Index out of range ({}..{})", i, e);
            }

            Ok(())
        }

        self.get_memory(base, |mut store, mem| {
            let data = mem.data_mut(&mut store);
            match v.dispatch() {
                VariantDispatch::ByteArray(v) => f(data, i, &*v.read()),
                VariantDispatch::Int32Array(v) => f(data, i, &*v.read()),
                VariantDispatch::Float32Array(v) => f(data, i, &*v.read()),
                VariantDispatch::Vector2Array(v) => f(data, i, &*v.read()),
                VariantDispatch::Vector3Array(v) => f(data, i, &*v.read()),
                VariantDispatch::ColorArray(v) => f(data, i, &*v.read()),
                _ => bail_with_site!("Unknown value"),
            }
        })
        .is_some()
    }

    #[method]
    fn get_array(
        &self,
        #[base] base: TRef<Reference>,
        i: usize,
        n: usize,
        t: i64,
    ) -> Option<Variant> {
        fn f<T: Copy + PoolElement>(s: &[u8], i: usize, n: usize) -> Result<PoolArray<T>, Error> {
            let l = n * size_of::<T>();
            let e = i + l;

            if let Some(s) = s.get(i..e) {
                let mut d = Vec::with_capacity(n);

                let ps = s.as_ptr();
                let pd = d.spare_capacity_mut().as_mut_ptr() as *mut u8;

                // SAFETY: Source and destination are of same size.
                // alignment of source should be enforced externally.
                unsafe {
                    ptr::copy_nonoverlapping(ps, pd, l);

                    #[cfg(target_endian = "big")]
                    if size_of::<T>() > 1 {
                        // SAFETY: destination size is l
                        for d in ptr::slice_from_raw_parts_mut(pd, l).chunks_mut(size_of::<T>()) {
                            debug_assert_eq!(d.len(), size_of::<T>());
                            d.reverse();
                        }
                    }

                    // SAFETY: value is initialized
                    d.set_len(n);
                }

                Ok(PoolArray::from_vec(d))
            } else {
                bail_with_site!("Index out of range ({}..{})", i, e);
            }
        }

        self.get_memory(base, |store, mem| {
            let data = mem.data(&store);
            match t {
                20 => f::<u8>(data, i, n).map(Variant::new), // PoolByteArray
                21 => f::<i32>(data, i, n).map(Variant::new), // PoolInt32Array
                22 => f::<f32>(data, i, n).map(Variant::new), // PoolFloat32Array
                24 => f::<Vector2>(data, i, n).map(Variant::new), // PoolVector2Array
                25 => f::<Vector3>(data, i, n).map(Variant::new), // PoolVector3Array
                26 => f::<Color>(data, i, n).map(Variant::new), // PoolColorArray
                ..=26 => bail_with_site!("Unsupported type ID {}", t),
                _ => bail_with_site!("Unknown type {}", t),
            }
        })
    }
}
