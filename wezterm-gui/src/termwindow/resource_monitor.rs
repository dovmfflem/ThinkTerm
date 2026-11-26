use super::remote_files::{
    invalidate_remote_connection_if_dead, remote_connection_key, remote_connection_manager,
};
use super::TermWindowNotif;
use crate::i18n::tr;
use crate::quad::TripleLayerQuadAllocator;
use crate::ui::DrawContext;
use crate::utilsprites::RenderMetrics;
use mux::domain::{DomainState, LocalDomain};
use mux::ssh::{RemoteSshDomain, SshConnectionConfig, SshConnectionStatus};
use mux::Mux;
use std::rc::Rc;
use std::time::{Duration, Instant};
use thinkterm_core::resource_metrics::{parse_nvidia_smi, Capacity, ResourceSample};
use wezterm_client::domain::ClientDomain;
use wezterm_font::{FontConfiguration, LoadedFont};
use window::{RectF, WindowOps};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const EXEC_TIMEOUT: Duration = Duration::from_secs(4);
const OUTPUT_LIMIT: usize = 8 * 1024;
const GIB: f64 = (1u64 << 30) as f64;
const GPU_QUERY: &str = "--query-gpu=index,memory.used,memory.total";
const GPU_FORMAT: &str = "--format=csv,noheader,nounits";

const LINUX_RESOURCES: &str = r#"
export LC_ALL=C
awk '/^cpu / { print; exit }' /proc/stat 2>/dev/null
awk '/^MemTotal:/ { total=$2 } /^MemAvailable:/ { available=$2 }
     END { if (total != "" && available != "") print "ram", total, available }' /proc/meminfo 2>/dev/null
df -Pk / 2>/dev/null | awk 'NR==2 && NF>=6 { print "disk", $2, $3 }'
"#;

fn status_font(fonts: &FontConfiguration) -> anyhow::Result<Rc<LoadedFont>> {
    fonts.command_palette_font_with_size_and_weight(11.0, 500)
}

fn status_vertical_padding(dpi: usize) -> usize {
    crate::ui::scale_ui_usize(8, dpi)
}

pub(super) fn status_height(fonts: &FontConfiguration, dpi: usize) -> usize {
    if !cfg!(windows) {
        return 0;
    }

    // Font metrics are already in physical pixels. Only the padding uses
    // ThinkTerm's chrome scale.
    let text_height = status_font(fonts)
        .map(|font| font.metrics().cell_height.get().ceil() as usize)
        .unwrap_or_else(|_| crate::ui::scale_ui_usize(40, dpi));
    text_height + 2 * status_vertical_padding(dpi)
}

#[derive(Default)]
pub(crate) struct ResourceMonitor {
    key: Option<String>,
    host: Option<String>,
    generation: u64,
    pending: bool,
    next_sample: Option<Instant>,
    sample: Option<ResourceSample>,
    cpu_percent: Option<u64>,
}

enum Collector {
    Local,
    Remote(SshConnectionConfig),
}

impl crate::TermWindow {
    fn resource_target(&self) -> Option<(String, Option<String>, Collector)> {
        let pane = self.get_active_pane_no_overlay()?;
        let domain = Mux::get().get_domain(pane.domain_id())?;
        if domain.state() != DomainState::Attached {
            return None;
        }

        let client = domain.downcast_ref::<ClientDomain>();
        let remote_ssh = domain.downcast_ref::<RemoteSshDomain>();
        if remote_ssh
            .is_some_and(|ssh| !matches!(ssh.connection_status(), SshConnectionStatus::Connected))
        {
            return None;
        }
        if remote_ssh.is_none()
            && (domain.downcast_ref::<LocalDomain>().is_some()
                || client.is_some_and(ClientDomain::is_local_session_host))
        {
            return Some(("local".to_string(), None, Collector::Local));
        }

        // The Files panel can target another project, so follow the terminal
        // pane's own SSH route.
        let config = if let Some(ssh) = remote_ssh {
            ssh.ssh_connection_config().clone()
        } else {
            client?.ssh_connection_config()?
        };
        let key = remote_connection_key(&format!("monitor:{}", domain.domain_id()), &config);
        let host = config.domain.remote_address.clone();
        Some((key, Some(host), Collector::Remote(config)))
    }

    pub(crate) fn update_resource_monitor(&mut self) {
        if !cfg!(windows) || self.content_view_foreground() {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        let target = self.resource_target();
        let source_key = target.as_ref().map(|(key, _, _)| key.as_str());
        if self.resource_monitor.key.as_deref() != source_key {
            self.resource_monitor = ResourceMonitor {
                key: source_key.map(str::to_owned),
                host: target.as_ref().and_then(|(_, host, _)| host.clone()),
                generation: self.resource_monitor.generation.wrapping_add(1),
                ..ResourceMonitor::default()
            };
            window.invalidate();
        }
        let Some((key, _, collector)) = target else {
            return;
        };
        let monitor = &mut self.resource_monitor;
        if monitor.pending
            || monitor
                .next_sample
                .is_some_and(|next| next > Instant::now())
        {
            return;
        }
        monitor.pending = true;
        let generation = monitor.generation;
        promise::spawn::spawn(async move {
            let sample = match collector {
                Collector::Local => read_local().await,
                Collector::Remote(config) => read_remote(key, config).await,
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let monitor = &mut term_window.resource_monitor;
                if monitor.generation != generation {
                    return;
                }
                let previous_cpu = monitor.sample.as_ref().and_then(|sample| sample.cpu);
                monitor.cpu_percent = sample
                    .as_ref()
                    .and_then(|sample| sample.cpu)
                    .zip(previous_cpu)
                    .and_then(|(current, previous)| current.busy_percent_since(previous));
                monitor.pending = false;
                let interval = if sample.is_some() {
                    SAMPLE_INTERVAL
                } else {
                    RETRY_INTERVAL
                };
                monitor.next_sample = Some(Instant::now() + interval);
                monitor.sample = sample;
                if let Some(window) = term_window.window.as_ref() {
                    window.invalidate();
                }
            })));
        })
        .detach();
    }

    pub(crate) fn resource_status_height(&self) -> f32 {
        status_height(&self.fonts, self.dimensions.dpi) as f32
    }

    pub(crate) fn resource_status_rect(&self) -> Option<RectF> {
        let height = self.resource_status_height();
        if height <= 0.0 {
            return None;
        }
        let left = self.tab_bar_left_edge() as f32;
        let width = self.terminal_viewport_right() - left;
        let bottom = self.dimensions.pixel_height as f32 - self.get_os_border().bottom.get() as f32;
        let top = (bottom - height).max(0.0);
        (width > 0.0 && bottom > top).then(|| euclid::rect(left, top, width, bottom - top))
    }

    pub(crate) fn paint_resource_status(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let Some(rect) = self.resource_status_rect() else {
            return Ok(());
        };
        let left = rect.min_x();
        let top = rect.min_y();
        let width = rect.size.width;
        let height = rect.size.height;
        let inset = crate::ui::scale_ui_usize(8, self.dimensions.dpi) as f32;
        if width <= inset * 2.0 {
            return Ok(());
        }

        let foreground = self.palette().foreground.to_linear();
        let background = self.terminal_default_background();
        self.filled_rectangle(layers, 0, rect, background)?;
        self.filled_rectangle(
            layers,
            0,
            euclid::rect(left, top, width, 1.0),
            foreground.mul_alpha(0.16),
        )?;
        let face = status_font(&self.fonts)?;
        let metrics = RenderMetrics::with_font_metrics(&face.metrics());
        if height < metrics.cell_size.height as f32 {
            return Ok(());
        }
        let state = self
            .render_state
            .as_ref()
            .expect("resource bar requires render state");
        let draw = DrawContext::new(state, self.dimensions, &metrics);
        let max_width = width - inset * 2.0;
        let text_top = top + ((height - metrics.cell_size.height as f32) / 2.0).max(0.0);
        draw.draw_text_on_layer(
            layers,
            2,
            &face,
            left + inset,
            text_top,
            &self.resource_monitor.label(),
            foreground,
            max_width,
        )?;
        Ok(())
    }
}

impl ResourceMonitor {
    fn label(&self) -> String {
        if self.sample.is_none() && !self.pending {
            return self.host.as_ref().map_or_else(
                || tr("resource-monitor-unavailable"),
                |host| format!("{host}  ·  {}", tr("resource-monitor-unavailable")),
            );
        }
        let sample = self.sample.as_ref();
        let host = self
            .host
            .clone()
            .unwrap_or_else(|| tr("resource-monitor-local"));
        let cpu = self
            .cpu_percent
            .map_or_else(|| "—".to_string(), |value| format!("{value}%"));
        let mut label = format!(
            "{host}  ·  CPU {cpu}  ·  RAM {}  ·  {} {}",
            capacity_label(sample.and_then(|sample| sample.ram)),
            tr("resource-monitor-disk"),
            capacity_label(sample.and_then(|sample| sample.disk))
        );
        for gpu in sample.into_iter().flat_map(|sample| &sample.gpus) {
            label.push_str(&format!(
                "  ·  GPU {} VRAM {}",
                gpu.index,
                capacity_label(Some(gpu.bytes))
            ));
        }
        label
    }
}

fn capacity_label(capacity: Option<Capacity>) -> String {
    capacity.map_or_else(
        || "—".to_string(),
        |value| {
            format!(
                "{:.1}/{:.1} GiB",
                value.used as f64 / GIB,
                value.total as f64 / GIB
            )
        },
    )
}

async fn read_remote(key: String, config: SshConnectionConfig) -> Option<ResourceSample> {
    let lease = remote_connection_manager()
        .acquire(key, config, true)
        .await
        .ok()?;
    let backend = lease.backend();
    let gpu_command = format!("LC_ALL=C nvidia-smi {GPU_QUERY} {GPU_FORMAT} 2>/dev/null");
    let (basic, gpu) = smol::future::zip(
        backend.run(LINUX_RESOURCES.to_string(), OUTPUT_LIMIT, EXEC_TIMEOUT),
        backend.run(gpu_command, OUTPUT_LIMIT, EXEC_TIMEOUT),
    )
    .await;
    let mut sample = match basic {
        Ok(run) if !run.cut => ResourceSample::parse_linux(&String::from_utf8_lossy(&run.out)),
        Err(err) => {
            invalidate_remote_connection_if_dead(
                lease.connection_key(),
                lease.connection_id(),
                &err,
            );
            ResourceSample::default()
        }
        _ => ResourceSample::default(),
    };
    if let Ok(run) = gpu {
        if run.status == Some(0) && !run.cut {
            sample.gpus = parse_nvidia_smi(&String::from_utf8_lossy(&run.out));
        }
    }
    sample.has_data().then_some(sample)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use thinkterm_core::resource_metrics::CpuTimes;
    use winapi::shared::minwindef::FILETIME;
    use winapi::um::fileapi::GetDiskFreeSpaceExW;
    use winapi::um::processthreadsapi::GetSystemTimes;
    use winapi::um::sysinfoapi::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use winapi::um::winbase::CREATE_NO_WINDOW;
    use winapi::um::winnt::ULARGE_INTEGER;

    fn cpu_times() -> Option<CpuTimes> {
        let mut idle = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut kernel = idle;
        let mut user = idle;
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return None;
        }
        let ticks =
            |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        Some(CpuTimes {
            total: ticks(kernel).checked_add(ticks(user))?,
            idle: ticks(idle),
        })
    }

    fn ram() -> Option<Capacity> {
        // All fields are integers; the API requires the structure size as input.
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return None;
        }
        Capacity::new(
            status.ullTotalPhys.checked_sub(status.ullAvailPhys)?,
            status.ullTotalPhys,
        )
    }

    fn disk() -> Option<Capacity> {
        let mut drive = std::env::var_os("SystemDrive").unwrap_or_else(|| "C:".into());
        drive.push("\\");
        let root: Vec<u16> = drive.encode_wide().chain(Some(0)).collect();
        let mut total: ULARGE_INTEGER = unsafe { std::mem::zeroed() };
        let mut available: ULARGE_INTEGER = unsafe { std::mem::zeroed() };
        if unsafe {
            GetDiskFreeSpaceExW(
                root.as_ptr(),
                &mut available,
                &mut total,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return None;
        }
        let (total, available) = unsafe { (*total.QuadPart(), *available.QuadPart()) };
        Capacity::new(total.checked_sub(available)?, total)
    }

    fn nvidia_smi() -> Option<PathBuf> {
        let system = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("nvidia-smi.exe"));
        let nv = std::env::var_os("ProgramFiles").map(|root| {
            PathBuf::from(root)
                .join("NVIDIA Corporation")
                .join("NVSMI")
                .join("nvidia-smi.exe")
        });
        system.into_iter().chain(nv).find(|path| path.is_file())
    }

    pub(super) async fn read() -> Option<ResourceSample> {
        use smol::process::windows::CommandExt;
        let mut sample = ResourceSample {
            cpu: cpu_times(),
            ram: ram(),
            disk: disk(),
            ..ResourceSample::default()
        };
        if let Some(path) = nvidia_smi() {
            let mut cmd = smol::process::Command::new(path);
            cmd.creation_flags(CREATE_NO_WINDOW)
                .args([GPU_QUERY, GPU_FORMAT])
                .kill_on_drop(true);
            let output = async { cmd.output().await.ok() };
            let timeout = async {
                smol::Timer::after(EXEC_TIMEOUT).await;
                None
            };
            if let Some(output) = smol::future::or(output, timeout).await {
                if output.status.success() {
                    sample.gpus = parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout));
                }
            }
        }
        sample.has_data().then_some(sample)
    }
}

#[cfg(windows)]
async fn read_local() -> Option<ResourceSample> {
    windows::read().await
}

#[cfg(not(windows))]
async fn read_local() -> Option<ResourceSample> {
    None
}
