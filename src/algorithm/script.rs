use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use rhai::{Dynamic, Engine, Map, Scope, AST};
use crate::api::{MarketData, TradeSignal};
use crate::algorithm::traits::Algorithm;

static SCRIPT_REGISTRY: OnceLock<Arc<RwLock<HashMap<String, AST>>>> = OnceLock::new();

pub fn init_script_registry(dir: &str) -> Arc<RwLock<HashMap<String, AST>>> {
    let map = scan_scripts(dir);
    let arc = Arc::new(RwLock::new(map));
    let _ = SCRIPT_REGISTRY.set(arc.clone());
    arc
}

pub async fn watch_algorithms(dir: &'static str, registry: Arc<RwLock<HashMap<String, AST>>>) {
    use tokio::time::{interval, Duration};

    let mut ticker = interval(Duration::from_millis(500));

    let mut known: HashMap<String, std::time::SystemTime> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "rhai"))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            let stem = e.path().file_stem()?.to_string_lossy().into_owned();
            Some((stem, mtime))
        })
        .collect();

    loop {
        ticker.tick().await;

        let current: HashMap<String, std::time::SystemTime> = match std::fs::read_dir(dir) {
            Err(_) => continue,
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "rhai"))
                .filter_map(|e| {
                    let mtime = e.metadata().ok()?.modified().ok()?;
                    let stem = e.path().file_stem()?.to_string_lossy().into_owned();
                    Some((stem, mtime))
                })
                .collect(),
        };

        let engine = make_engine();

        for (stem, mtime) in &current {
            let is_new = !known.contains_key(stem.as_str());
            let is_modified = known.get(stem.as_str()).is_some_and(|m| m != mtime);
            if is_new || is_modified {
                let path = format!("{}/{}.rhai", dir, stem);
                match engine.compile_file(path.into()) {
                    Ok(ast) => {
                        let action = if is_new { "Loaded" } else { "Reloaded" };
                        if let Ok(mut guard) = registry.write() {
                            guard.insert(stem.clone(), ast);
                        }
                        crate::logger::log("[ALGO]", &format!("{}: {}", action, stem));
                    }
                    Err(e) => {
                        crate::logger::log("[ALGO]", &format!("Compile error in {}: {}", stem, e));
                    }
                }
            }
        }

        for stem in known.keys() {
            if !current.contains_key(stem.as_str()) {
                if let Ok(mut guard) = registry.write() {
                    guard.remove(stem.as_str());
                }
                crate::logger::log("[ALGO]", &format!("Removed: {}", stem));
            }
        }

        known = current;
    }
}

fn scan_scripts(dir: &str) -> HashMap<String, AST> {
    let engine = make_engine();
    let mut map = HashMap::new();

    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => {
            crate::logger::log("[ALGO]", &format!("algorithms/ directory not found at '{}' — no scripts loaded.", dir));
            return map;
        }
    };

    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "rhai") {
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            match engine.compile_file(path.clone()) {
                Ok(ast) => {
                    map.insert(stem, ast);
                }
                Err(e) => {
                    crate::logger::log("[ALGO]", &format!("Compile error in {}: {}", path.display(), e));
                }
            }
        }
    }

    map
}

fn make_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(100_000);
    engine.set_max_call_levels(32);
    engine
}

pub struct ScriptAlgorithm {
    name: String,
    engine: Engine,
    ast: AST,
    scope: Scope<'static>,
}

impl ScriptAlgorithm {
    pub fn new(
        name: String,
        ast: AST,
        options: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let engine = make_engine();
        let mut scope = Scope::new();

        let mut rhai_options = Map::new();
        for (k, v) in options {
            rhai_options.insert(k.clone().into(), Dynamic::from(v.clone()));
        }
        scope.push_constant("options", rhai_options);

        let _ = engine
            .eval_ast_with_scope::<Dynamic>(&mut scope, &ast)
            .map_err(|e| format!("Script init error in '{}': {}", name, e))?;

        Ok(ScriptAlgorithm { name, engine, ast, scope })
    }
}

fn parse_signal(map: &Map) -> Option<TradeSignal> {
    let kind = map.get("kind")?.clone().into_string().ok()?;
    let price = map.get("price")?.as_float().ok()?;
    let qty = map.get("qty")?.as_float().ok()?;
    let reason = map
        .get("reason")
        .and_then(|r| r.clone().into_string().ok())
        .unwrap_or_default();
    match kind.as_str() {
        "buy" => Some(TradeSignal::Buy { price, quantity: qty, reason, price_decimals: 8 }),
        "sell" => Some(TradeSignal::Sell { price, quantity: qty, reason, price_decimals: 8 }),
        _ => None,
    }
}

impl Algorithm for ScriptAlgorithm {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_tick(&mut self, tick: &MarketData) -> Vec<TradeSignal> {
        let args = (tick.last_price, tick.bid, tick.ask, tick.volume);
        match self.engine.call_fn::<Dynamic>(&mut self.scope, &self.ast, "on_tick", args) {
            Ok(result) => {
                if result.is_unit() {
                    vec![]
                } else if let Some(map) = result.clone().try_cast::<Map>() {
                    parse_signal(&map).map(|s| vec![s]).unwrap_or_default()
                } else if let Ok(arr) = result.into_array() {
                    arr.iter()
                        .filter_map(|v| v.clone().try_cast::<Map>())
                        .filter_map(|m| parse_signal(&m))
                        .collect()
                } else {
                    vec![]
                }
            }
            Err(e) => {
                crate::logger::log(
                    &format!("[SCRIPT:{}]", self.name),
                    &format!("on_tick error: {}", e),
                );
                vec![]
            }
        }
    }

    fn summary(&self) -> Option<String> {
        let mut scope_clone = self.scope.clone();
        self.engine
            .call_fn::<String>(&mut scope_clone, &self.ast, "summary", ())
            .ok()
    }

    fn on_reconnect(&mut self) {
        let options = self
            .scope
            .get_value::<Map>("options")
            .unwrap_or_default();
        self.scope = Scope::new();
        self.scope.push_constant("options", options);
        drop(self.engine.eval_ast_with_scope::<Dynamic>(&mut self.scope, &self.ast));
        crate::logger::log(
            &format!("[SCRIPT:{}]", self.name),
            "Reconnected — script state reset to initial values.",
        );
    }
}

pub fn build_script_algorithm(
    name: &str,
    options: &HashMap<String, String>,
) -> Option<Result<Box<dyn Algorithm>, String>> {
    let registry = SCRIPT_REGISTRY.get()?;
    let ast = {
        let guard = registry.read().ok()?;
        guard.get(name)?.clone()
    };
    Some(
        ScriptAlgorithm::new(name.to_string(), ast, options)
            .map(|s| Box::new(s) as Box<dyn Algorithm>),
    )
}
