//! Uploaded lab verifiers: the store's API and the sandboxed checker call (docs/design/verifier-sandbox.md).

use super::{e500, ApiError, Manager, R};
use drumsd_core::verifiers::{is_sha256, Store, StoreError};
use drumsd_linux::{cgroup, vsandbox};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

fn store_err(e: StoreError) -> ApiError {
    match e {
        StoreError::Invalid(s, m) => ApiError::new(s, m),
        StoreError::NotFound(m) => ApiError::new(404, m).with(json!({"code": "verifier_missing"})),
        StoreError::Corrupt(m) => ApiError::new(500, m).with(json!({"code": "verifier_corrupt"})),
        StoreError::Io(m) => ApiError::new(500, m),
    }
}

/// The hashes a task's evaluator names: the primary's `verifier_sha256` and its redundant checker's (if any).
pub fn task_verifier_hashes(task: &Value) -> Vec<(&'static str, Value)> {
    let ev = &task["evaluator"];
    let mut out = vec![];
    if ev["func"] != "drums_verifier" {
        return out;
    }
    if let Some(h) = ev.get("verifier_sha256").filter(|v| !v.is_null()) {
        out.push(("evaluator.verifier_sha256", h.clone()));
    }
    if let Some(h) = ev["redundant"].get("verifier_sha256").filter(|v| !v.is_null()) {
        out.push(("evaluator.redundant.verifier_sha256", h.clone()));
    }
    out
}

impl Manager {
    fn store(&self) -> R<&Store> {
        self.vstore
            .as_ref()
            .ok_or_else(|| ApiError::new(503, format!("the verifier store {} is unavailable on this host", self.cfg.verifiers.store_dir.display())))
    }

    pub fn upload_verifier(&self, src: &[u8], name: Option<&str>) -> R<Value> {
        let (rec, created) = self.store()?.put(src, name, "upload", None, drumsd_linux::now_epoch()).map_err(store_err)?;
        self.event(json!({"verifier_upload": {"sha256": rec.sha256, "bytes": rec.bytes, "created": created}}));
        let mut v = serde_json::to_value(&rec).unwrap();
        v["created"] = json!(created);
        Ok(v)
    }

    pub fn list_verifiers(&self) -> R<Value> {
        Ok(json!(self.store()?.list()))
    }

    pub fn get_verifier(&self, sha: &str) -> R<Value> {
        Ok(serde_json::to_value(self.store()?.get(sha).map_err(store_err)?).unwrap())
    }

    pub fn delete_verifier(&self, sha: &str) -> R<Value> {
        let existed = self.store()?.delete(sha).map_err(store_err)?;
        Ok(json!({"sha256": sha, "deleted": existed}))
    }

    /// A task that names a verifier by hash is refused before any episode runs unless the store has it.
    pub fn check_task_verifiers(&self, task: &Value) -> R<()> {
        for (field, h) in task_verifier_hashes(task) {
            let Some(sha) = h.as_str().filter(|s| is_sha256(s)) else {
                return Err(
                    ApiError::new(400, format!("task: {field} must be 64 lowercase hex characters")).with(json!({"code": "invalid_task", "field": field}))
                );
            };
            match self.store()?.get(sha) {
                Ok(_) => {}
                Err(StoreError::NotFound(_)) => {
                    return Err(ApiError::new(
                        422,
                        format!("task names verifier {sha} ({field}), which this host does not have: upload it with POST /v1/verifiers"),
                    )
                    .with(json!({"code": "verifier_missing", "verifier_sha256": sha, "field": field})))
                }
                Err(e) => return Err(store_err(e)),
            }
        }
        Ok(())
    }

    /// One sandboxed checker call for the OSWorld helper of a running evaluate, through that call's capability URL.
    /// Body: {"sha256", "function", "options", "deadline_s", "named"?}. `named`: a verifier the helper loaded by file
    /// name (sandbox_named), imported into the store by its hash on first use.
    pub async fn sandbox_verify(self: &Arc<Self>, token: &str, body: Value) -> R<Value> {
        let target = self.provision_tokens.lock().unwrap().get(token).cloned().ok_or_else(|| ApiError::new(403, "unknown or expired provisioning token"))?;
        let store = self.store()?.clone();
        let sha = body["sha256"].as_str().filter(|s| is_sha256(s)).ok_or_else(|| ApiError::new(400, "sha256: 64 lowercase hex characters"))?.to_string();
        if let (Some(name), Err(StoreError::NotFound(_))) = (body["named"].as_str(), store.get(&sha)) {
            self.import_named(&store, name, &sha)?;
        }
        let file = store.path(&sha).map_err(store_err)?;
        let vc = &self.cfg.verifiers;
        let (python, ro_paths) =
            vc.python_and_paths(self.cfg.osw.as_ref()).ok_or_else(|| ApiError::new(503, "no checker python configured (verifiers.python or osw.python)"))?;
        let function = body["function"].as_str().unwrap_or("reward").to_string();
        if !drumsd_core::verifiers::valid_name(&function) {
            return Err(ApiError::new(400, "function: letters, digits and _ only"));
        }
        let spec = vsandbox::Spec {
            python,
            ro_paths,
            verifier: file,
            sha256: sha.clone(),
            function,
            options: body.get("options").cloned().filter(|o| o.is_object()).unwrap_or(json!({})),
            task: body.get("task").cloned().unwrap_or(Value::Null),
            uid: vc.uid,
            gid: vc.gid,
            cgroup: PathBuf::new(),
            root: PathBuf::new(),
            budget: vc.budget.clone(),
            wall_s: vc.budget.wall_for(body["deadline_s"].as_f64()),
        };
        let confine = vsandbox::Confine::Sandbox {
            exe: self.exe.clone(),
            cgroup_parent: vc.cgroup_parent.clone().unwrap_or_else(|| cgroup::pool().join("drums-verify")),
            run_dir: vc.run_dir.clone(),
        };
        let desktop = vsandbox::GuestDesktop { port: self.cfg.port_base + target.num as u16 };
        let v = tokio::task::spawn_blocking(move || vsandbox::run(&confine, &spec, &desktop)).await.map_err(e500)?;
        let class = v["error_class"].as_str().unwrap_or("unknown").to_string();
        self.metrics.inc("drumsd_verifier_runs_total", &[("error_class", class.as_str())], 1.0);
        self.event(json!({"verifier_run": {"slot_num": target.num, "sha256": sha, "error_class": class, "seconds": v["seconds"], "cpu_s": v.get("cpu_s"), "memory_peak_mib": v.get("memory_peak_mib")}}));
        Ok(v)
    }

    /// Import NAME.py from the named verifiers dir into the store, if (and only if) it hashes to `sha`.
    fn import_named(&self, store: &Store, name: &str, sha: &str) -> R<()> {
        if !drumsd_core::verifiers::valid_name(name) {
            return Err(ApiError::new(400, "named: letters, digits and _ only"));
        }
        let dir = self.cfg.osw.as_ref().and_then(|o| o.verifiers_dir.clone()).unwrap_or_else(|| PathBuf::from("/etc/drums/verifiers"));
        let src = std::fs::read(dir.join(format!("{name}.py"))).map_err(|e| ApiError::new(404, format!("named verifier {name}: {e}")))?;
        if drumsd_core::verifiers::sha256_hex(&src) != sha {
            return Err(ApiError::new(409, format!("named verifier {name} changed while it was loaded (hash mismatch)")));
        }
        store.put(&src, Some(name), "named", None, drumsd_linux::now_epoch()).map_err(store_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lab_tasks_name_hashes() {
        let h = "a".repeat(64);
        let t = json!({"evaluator": {"func": "drums_verifier", "verifier_sha256": h, "redundant": {"function": "alt", "verifier_sha256": h}}});
        assert_eq!(task_verifier_hashes(&t).len(), 2);
        let t = json!({"evaluator": {"func": "drums_verifier", "verifier": "drums_t6"}});
        assert!(task_verifier_hashes(&t).is_empty());
        let t = json!({"evaluator": {"func": "compare_table", "verifier_sha256": h}});
        assert!(task_verifier_hashes(&t).is_empty());
    }
}
