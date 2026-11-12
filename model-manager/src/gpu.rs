//! Discovery of the host's GPUs, used to populate the recipe editor and to
//! stop a second container from landing on a card that's already in use.

use serde::Serialize;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub vram_bytes: u64,
}

/// Runs `nvidia-smi` once at startup. Returns an empty list (not an error) on
/// any hosts without an NVIDIA driver — GPU features just show as unavailable.
pub async fn discover() -> Vec<GpuInfo> {
    let output = match Command::new("nvidia-smi")
        .args(["--query-gpu=index,name,memory.total", "--format=csv,noheader,nounits"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::info!(
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "nvidia-smi exited non-zero — no GPUs will be offered"
            );
            return Vec::new();
        }
        Err(err) => {
            tracing::info!(%err, "nvidia-smi not available — no GPUs will be offered");
            return Vec::new();
        }
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(',').map(str::trim);
            let index = parts.next()?.parse().ok()?;
            let name = parts.next()?.to_string();
            let mib: u64 = parts.next()?.parse().ok()?;
            Some(GpuInfo { index, name, vram_bytes: mib * 1024 * 1024 })
        })
        .collect()
}
