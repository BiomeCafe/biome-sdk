//! Reference host for Rules ABI v1.
//!
//! Loads a `wasm32-unknown-unknown` module built from a `biome-rules` crate
//! and calls it the way the match Durable Object does: write inputs into the
//! module's memory, call an export, read the result buffer, free.
//!
//! What it enforces (the same things the production host must):
//!
//! - **No imports.** A rules module is a pure function library; a module
//!   that asks for anything from its host is refused. That is the sandbox.
//! - **The ten ABI exports exist with exactly the ABI's signatures** (all
//!   ten are type-checked at load, so a wrong `br_apply` is refused by
//!   `validate` instead of failing on the first move) and `br_abi()` is a
//!   version this host speaks.
//! - **A fuel budget per call**, so a rules bug that loops forever is a
//!   failed call, not a hung host.
//! - **A memory cap per call** ([`MAX_MEMORY_BYTES`]), so a module cannot
//!   make the host reserve gigabytes, at load (`validate` runs modules
//!   nobody has reviewed yet) or mid-match.
//! - **A fresh instance per call.** State goes in and comes out as bytes;
//!   nothing survives between calls, exactly like an object that may be
//!   evicted between moves.
//!
//! Errors come in two layers: [`HostError`] means the *module* misbehaved
//! (trap, bad buffer, out of fuel) and the caller keeps its old state;
//! `Err(Reject)` inside an `Ok` is the game saying no to a move.

use biome_rules::abi::ABI_VERSION;
use biome_rules::wire::{self, ApplyOk};
use biome_rules::{Meta, Outcome, Reject, Seat, SeatInfo, Turn};
use wasmi::{Config, Engine, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TrapCode};

/// Default instruction budget for one call. A tic-tac-toe move is a few
/// thousand; a card-game move with a shuffle is well under a million.
pub const DEFAULT_FUEL: u64 = 50_000_000;

/// Most seats a game may declare. Seat 254 is reserved as "nobody" and 255
/// as the spectator, so indices stop at 253.
pub const MAX_SEATS: u8 = 254;

/// Table elements a module may hold (function tables for indirect calls).
pub const MAX_TABLE_ELEMENTS: usize = 100_000;

/// Largest module accepted (bytes): 4 MiB, as game-publish and the
/// game_release_files table. Every module of every game shares the match
/// host's worker size limit (docs/GAME_PUBLISHING_SPEC.md §8).
pub const MAX_MODULE_BYTES: usize = 4 * 1024 * 1024;

/// Largest single result buffer accepted (bytes).
pub const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;

/// Linear memory a module may hold during one call (initial + grown). A
/// card or board game needs a few hundred kilobytes; this is generous and
/// still far below what would hurt the host. A module that declares more up
/// front fails to instantiate; one that grows past it gets a failed
/// `memory.grow`, which the Rust allocator turns into a trap.
pub const MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

const EXPORTS: [&str; 10] = [
    "br_abi", "br_alloc", "br_free", "br_meta", "br_init", "br_apply", "br_timeout", "br_to_move",
    "br_view", "br_outcome",
];

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// Not a rules module this host will run (bad wasm, imports, missing
    /// export, wrong ABI version, unreadable meta).
    #[error("invalid rules module: {0}")]
    Invalid(String),
    /// The module trapped (a panic in the game's code, out-of-bounds, …).
    #[error("rules module trapped: {0}")]
    Trap(String),
    /// The call ran out of its instruction budget.
    #[error("rules call exceeded its fuel budget")]
    OutOfFuel,
    /// The module returned bytes that don't follow the wire layout.
    #[error("rules module returned a malformed buffer: {0}")]
    Wire(String),
}

/// What a call returns: the host-level result, then the game-level one.
pub type Call<T> = Result<Result<T, Reject>, HostError>;

/// A validated rules module, ready to be called.
pub struct RulesModule {
    engine: Engine,
    module: Module,
    meta: Meta,
    fuel: u64,
}

impl core::fmt::Debug for RulesModule {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RulesModule").field("meta", &self.meta).field("fuel", &self.fuel).finish()
    }
}

struct Session {
    store: Store<StoreLimits>,
    instance: Instance,
    memory: Memory,
}

fn trap(e: wasmi::Error) -> HostError {
    // Prefer the structured code. wasmi also reports fuel exhaustion through
    // a few non-trap error kinds it does not expose, so keep the message
    // check as the fallback for those.
    if e.as_trap_code() == Some(TrapCode::OutOfFuel) {
        return HostError::OutOfFuel;
    }
    let msg = e.to_string();
    if msg.to_ascii_lowercase().contains("fuel") {
        HostError::OutOfFuel
    } else {
        HostError::Trap(msg)
    }
}

/// Resolve every ABI export with its exact signature. Called at load so a
/// module with, say, a five-argument `br_apply` never passes validation.
fn check_signatures(s: &Session) -> Result<(), HostError> {
    let bad = |name: &str, e: wasmi::Error| HostError::Invalid(format!("export `{name}` has the wrong signature: {e}"));
    let i = &s.instance;
    let st = &s.store;
    i.get_typed_func::<(), u32>(st, "br_abi").map_err(|e| bad("br_abi", e))?;
    i.get_typed_func::<u32, u32>(st, "br_alloc").map_err(|e| bad("br_alloc", e))?;
    i.get_typed_func::<(u32, u32), ()>(st, "br_free").map_err(|e| bad("br_free", e))?;
    i.get_typed_func::<(), u64>(st, "br_meta").map_err(|e| bad("br_meta", e))?;
    i.get_typed_func::<(u32, u32, u32, u32, u32), u64>(st, "br_init").map_err(|e| bad("br_init", e))?;
    i.get_typed_func::<(u32, u32, u32, u32, u32, u32), u64>(st, "br_apply").map_err(|e| bad("br_apply", e))?;
    i.get_typed_func::<(u32, u32, u32, u32), u64>(st, "br_timeout").map_err(|e| bad("br_timeout", e))?;
    i.get_typed_func::<(u32, u32), u64>(st, "br_to_move").map_err(|e| bad("br_to_move", e))?;
    i.get_typed_func::<(u32, u32, u32), u64>(st, "br_view").map_err(|e| bad("br_view", e))?;
    i.get_typed_func::<(u32, u32), u64>(st, "br_outcome").map_err(|e| bad("br_outcome", e))?;
    Ok(())
}

impl RulesModule {
    /// Validate and load a module with the default fuel budget.
    pub fn load(wasm: &[u8]) -> Result<Self, HostError> {
        Self::load_with_fuel(wasm, DEFAULT_FUEL)
    }

    /// Validate and load a module with a specific per-call fuel budget.
    pub fn load_with_fuel(wasm: &[u8], fuel: u64) -> Result<Self, HostError> {
        if wasm.len() > MAX_MODULE_BYTES {
            return Err(HostError::Invalid(format!("{} bytes exceeds the {} byte limit", wasm.len(), MAX_MODULE_BYTES)));
        }
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).map_err(|e| HostError::Invalid(format!("not a valid module: {e}")))?;

        if let Some(import) = module.imports().next() {
            return Err(HostError::Invalid(format!(
                "module imports `{}.{}`; rules modules may not import anything",
                import.module(),
                import.name()
            )));
        }
        for name in EXPORTS {
            if !module.exports().any(|e| e.name() == name) {
                return Err(HostError::Invalid(format!("missing export `{name}`")));
            }
        }
        if !module.exports().any(|e| e.name() == "memory") {
            return Err(HostError::Invalid("missing export `memory`".into()));
        }

        let mut this = Self {
            engine,
            module,
            meta: Meta { abi: 0, name: String::new(), rules_version: String::new(), min_seats: 0, max_seats: 0 },
            fuel,
        };

        let mut s = this.session()?;
        check_signatures(&s)?;
        let abi = s
            .instance
            .get_typed_func::<(), u32>(&s.store, "br_abi")
            .map_err(|e| HostError::Invalid(format!("br_abi: {e}")))?
            .call(&mut s.store, ())
            .map_err(trap)?;
        if abi != ABI_VERSION {
            return Err(HostError::Invalid(format!("module speaks ABI {abi}, this host speaks {ABI_VERSION}")));
        }
        let packed = s
            .instance
            .get_typed_func::<(), u64>(&s.store, "br_meta")
            .map_err(|e| HostError::Invalid(format!("br_meta: {e}")))?
            .call(&mut s.store, ())
            .map_err(trap)?;
        let meta_bytes = s.take(packed)?;
        let meta: Meta = serde_json::from_slice(&meta_bytes).map_err(|e| HostError::Invalid(format!("meta json: {e}")))?;
        if meta.abi != abi {
            return Err(HostError::Invalid("br_meta and br_abi disagree".into()));
        }
        // Seat numbers stop at 253: 254 is "nobody" (what an out-of-range seat
        // argument is mapped to) and 255 is the spectator.
        if meta.min_seats == 0 || meta.min_seats > meta.max_seats || meta.max_seats > MAX_SEATS {
            return Err(HostError::Invalid(format!(
                "seat range {}..={} makes no sense (at most {MAX_SEATS} seats)",
                meta.min_seats, meta.max_seats
            )));
        }
        if meta.rules_version.is_empty() || meta.name.is_empty() {
            return Err(HostError::Invalid("meta has an empty name or rules_version".into()));
        }
        this.meta = meta;
        Ok(this)
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    fn session(&self) -> Result<Session, HostError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(MAX_MEMORY_BYTES)
            .memories(1)
            .tables(1)
            .table_elements(MAX_TABLE_ELEMENTS)
            .instances(1)
            .build();
        let mut store = Store::new(&self.engine, limits);
        store.limiter(|limits| limits);
        store.set_fuel(self.fuel).map_err(|e| HostError::Invalid(format!("fuel: {e}")))?;
        let linker = Linker::<StoreLimits>::new(&self.engine);
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| HostError::Invalid(format!("instantiate: {e}")))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| HostError::Invalid("no exported memory".into()))?;
        Ok(Session { store, instance, memory })
    }

    /// Build the initial state. `config` is the game's postcard config.
    pub fn init(&self, config: &[u8], seats: &[SeatInfo], seed: &[u8; 32]) -> Call<Vec<u8>> {
        let seats_json = serde_json::to_vec(seats).map_err(|e| HostError::Wire(e.to_string()))?;
        let mut s = self.session()?;
        let (c, cl) = s.put(config)?;
        let (j, jl) = s.put(&seats_json)?;
        let (sd, _) = s.put(seed)?;
        let f = s
            .instance
            .get_typed_func::<(u32, u32, u32, u32, u32), u64>(&s.store, "br_init")
            .map_err(|e| HostError::Invalid(format!("br_init: {e}")))?;
        let packed = f.call(&mut s.store, (c, cl, j, jl, sd)).map_err(trap)?;
        let buf = s.take(packed)?;
        split(&buf).map(|r| r.map(|p| p.to_vec()))
    }

    /// Validate and apply a move.
    pub fn apply(&self, state: &[u8], seat: Seat, mv: &[u8], seed: &[u8; 32]) -> Call<ApplyOk> {
        let mut s = self.session()?;
        let (st, stl) = s.put(state)?;
        let (m, ml) = s.put(mv)?;
        let (sd, _) = s.put(seed)?;
        let f = s
            .instance
            .get_typed_func::<(u32, u32, u32, u32, u32, u32), u64>(&s.store, "br_apply")
            .map_err(|e| HostError::Invalid(format!("br_apply: {e}")))?;
        let packed = f.call(&mut s.store, (st, stl, seat.0 as u32, m, ml, sd)).map_err(trap)?;
        let buf = s.take(packed)?;
        decode_apply(&buf)
    }

    /// `seat` ran out of clock.
    pub fn timeout(&self, state: &[u8], seat: Seat, seed: &[u8; 32]) -> Call<ApplyOk> {
        let mut s = self.session()?;
        let (st, stl) = s.put(state)?;
        let (sd, _) = s.put(seed)?;
        let f = s
            .instance
            .get_typed_func::<(u32, u32, u32, u32), u64>(&s.store, "br_timeout")
            .map_err(|e| HostError::Invalid(format!("br_timeout: {e}")))?;
        let packed = f.call(&mut s.store, (st, stl, seat.0 as u32, sd)).map_err(trap)?;
        let buf = s.take(packed)?;
        decode_apply(&buf)
    }

    pub fn to_move(&self, state: &[u8]) -> Call<Turn> {
        let buf = self.state_call("br_to_move", state)?;
        match split(&buf)? {
            Ok(p) => wire::decode_turn(p).map(Ok).map_err(|e| HostError::Wire(e.0)),
            Err(r) => Ok(Err(r)),
        }
    }

    pub fn outcome(&self, state: &[u8]) -> Call<Option<Outcome>> {
        let buf = self.state_call("br_outcome", state)?;
        match split(&buf)? {
            Ok(p) => wire::decode_outcome(p).map(Ok).map_err(|e| HostError::Wire(e.0)),
            Err(r) => Ok(Err(r)),
        }
    }

    /// The view bytes for one seat (`Seat::SPECTATOR` for the public view).
    pub fn view(&self, state: &[u8], seat: Seat) -> Call<Vec<u8>> {
        let mut s = self.session()?;
        let (st, stl) = s.put(state)?;
        let f = s
            .instance
            .get_typed_func::<(u32, u32, u32), u64>(&s.store, "br_view")
            .map_err(|e| HostError::Invalid(format!("br_view: {e}")))?;
        let packed = f.call(&mut s.store, (st, stl, seat.0 as u32)).map_err(trap)?;
        let buf = s.take(packed)?;
        split(&buf).map(|r| r.map(|p| p.to_vec()))
    }

    fn state_call(&self, export: &str, state: &[u8]) -> Result<Vec<u8>, HostError> {
        let mut s = self.session()?;
        let (st, stl) = s.put(state)?;
        let f = s
            .instance
            .get_typed_func::<(u32, u32), u64>(&s.store, export)
            .map_err(|e| HostError::Invalid(format!("{export}: {e}")))?;
        let packed = f.call(&mut s.store, (st, stl)).map_err(trap)?;
        s.take(packed)
    }
}

fn split(buf: &[u8]) -> Result<Result<&[u8], Reject>, HostError> {
    wire::split_result(buf).map_err(|e| HostError::Wire(e.0))
}

fn decode_apply(buf: &[u8]) -> Call<ApplyOk> {
    match split(buf)? {
        Ok(p) => wire::decode_apply_ok(p).map(Ok).map_err(|e| HostError::Wire(e.0)),
        Err(r) => Ok(Err(r)),
    }
}

impl Session {
    /// Copy `bytes` into module memory; returns `(ptr, len)`.
    fn put(&mut self, bytes: &[u8]) -> Result<(u32, u32), HostError> {
        let len = u32::try_from(bytes.len()).map_err(|_| HostError::Wire("input too large".into()))?;
        let alloc = self
            .instance
            .get_typed_func::<u32, u32>(&self.store, "br_alloc")
            .map_err(|e| HostError::Invalid(format!("br_alloc: {e}")))?;
        let ptr = alloc.call(&mut self.store, len).map_err(trap)?;
        if len > 0 {
            self.memory
                .write(&mut self.store, ptr as usize, bytes)
                .map_err(|e| HostError::Trap(format!("write into module memory: {e}")))?;
        }
        Ok((ptr, len))
    }

    /// Read and free a returned `(ptr << 32) | len` buffer.
    fn take(&mut self, packed: u64) -> Result<Vec<u8>, HostError> {
        let ptr = (packed >> 32) as u32;
        let len = (packed & 0xffff_ffff) as u32;
        if len as usize > MAX_RESULT_BYTES {
            return Err(HostError::Wire(format!("result of {len} bytes exceeds the limit")));
        }
        let mut out = vec![0u8; len as usize];
        if len > 0 {
            self.memory
                .read(&self.store, ptr as usize, &mut out)
                .map_err(|e| HostError::Wire(format!("result buffer out of bounds: {e}")))?;
            let free = self
                .instance
                .get_typed_func::<(u32, u32), ()>(&self.store, "br_free")
                .map_err(|e| HostError::Invalid(format!("br_free: {e}")))?;
            free.call(&mut self.store, (ptr, len)).map_err(trap)?;
        }
        Ok(out)
    }
}

/// Check that `wasm` is a rules module this host would run, and return what
/// it says about itself. Used by the publish tool before upload.
pub fn validate(wasm: &[u8]) -> Result<Meta, HostError> {
    RulesModule::load(wasm).map(|m| m.meta().clone())
}
