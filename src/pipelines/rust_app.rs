//! Rust application pipeline.

use std::borrow::Cow;
use std::iter::Iterator;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use cargo_lock::Lockfile;
use cargo_metadata::Artifact;
use futures::future::join_all;
use nipper::Document;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{LinkAttrs, TrunkLinkPipelineOutput};
use super::{ATTR_HREF, SNIPPETS_DIR};
use crate::common::{self, copy_dir_recursive, path_exists};
use crate::config::{CargoMetadata, ConfigOptsTools, RtcBuild};
use crate::tools::{self, Application};


/// A Rust application pipeline.
pub struct RustApp {
    /// The ID of this pipeline's source HTML element.
    id: Option<usize>,
    /// Runtime config.
    cfg: Arc<RtcBuild>,
    /// Space or comma separated list of cargo features to activate.
    cargo_features: Option<String>,
    /// All metadata associated with the target Cargo project.
    manifest: CargoMetadata,
    /// An optional channel to be used to communicate paths to ignore back to the watcher.
    ignore_chan: Option<mpsc::Sender<PathBuf>>,
    /// An optional binary name which will cause cargo & wasm-bindgen to process only the target
    /// binary.
    bin: Option<String>,
    /// An option to instruct wasm-bindgen to preserve debug info in the final WASM output, even
    /// for `--release` mode.
    keep_debug: bool,
    /// An option to instruct wasm-bindgen to not demangle Rust symbol names.
    no_demangle: bool,
    /// An optional optimization setting that enables wasm-opt. Can be nothing, `0` (default), `1`,
    /// `2`, `3`, `4`, `s or `z`. Using `0` disables wasm-opt completely.
    wasm_opt: WasmOptLevel,
    /// Worker names
    worker_names: Vec<String>,
}

/// Cargo build output
#[derive (Debug)]
struct CargoBuildOutput {
    /// path to wasm file
    wasm: PathBuf,
    /// hashed name
    hashed_name: String,
    /// indicates if
    is_worker: bool,
    worker_name: Option<String>,
}



impl RustApp {
    pub const TYPE_RUST_APP: &'static str = "rust";

    pub async fn new(
        cfg: Arc<RtcBuild>, html_dir: Arc<PathBuf>, ignore_chan: Option<mpsc::Sender<PathBuf>>, attrs: LinkAttrs, id: usize,
    ) -> Result<Self> {
        // Build the path to the target asset.
        let manifest_href = attrs
            .get(ATTR_HREF)
            .map(|attr| {
                let mut path = PathBuf::new();
                path.extend(attr.split('/'));
                if !path.is_absolute() {
                    path = html_dir.join(path);
                }
                if !path.ends_with("Cargo.toml") {
                    path = path.join("Cargo.toml");
                }
                path
            })
            .unwrap_or_else(|| html_dir.join("Cargo.toml"));
        let bin = attrs.get("data-bin").map(|val| val.to_string());
        let cargo_features = attrs.get("data-cargo-features").map(|val| val.to_string());
        let keep_debug = attrs.contains_key("data-keep-debug");
        let no_demangle = attrs.contains_key("data-no-demangle");
        let wasm_opt = attrs
            .get("data-wasm-opt")
            .map(|val| val.parse())
            .transpose()?
            .unwrap_or_else(|| if cfg.release { Default::default() } else { WasmOptLevel::Off });
        let manifest = CargoMetadata::new(&manifest_href).await?;
        let id = Some(id);

        let worker_names = match attrs.get("data-workers") {
            None => Vec::new(),
            Some(wn) => wn.split(",").map(|s| s.trim().to_string()).collect(),
        };


        Ok(Self {
            id,
            cfg,
            cargo_features,
            manifest,
            ignore_chan,
            bin,
            keep_debug,
            no_demangle,
            wasm_opt,
            worker_names,
        })
    }

    pub async fn new_default(cfg: Arc<RtcBuild>, html_dir: Arc<PathBuf>, ignore_chan: Option<mpsc::Sender<PathBuf>>) -> Result<Self> {
        let path = html_dir.join("Cargo.toml");
        let manifest = CargoMetadata::new(&path).await?;
        Ok(Self {
            id: None,
            cfg,
            cargo_features: None,
            manifest,
            ignore_chan,
            bin: None,
            keep_debug: false,
            no_demangle: false,
            wasm_opt: WasmOptLevel::Off,
            worker_names: Vec::new(),
        })
    }

    /// Spawn a new pipeline.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn spawn(self) -> JoinHandle<Result<TrunkLinkPipelineOutput>> {
        tokio::spawn(self.build())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn build(mut self) -> Result<TrunkLinkPipelineOutput> {
        let cargo_build_output = self.cargo_build().await?;
        tracing::info!("cargo_output = {:?}", cargo_build_output);
        let mut wasm_files = Vec::<RustAppOutputWasm>::new();
        for i in 0..cargo_build_output.len() {
            let cbo = &cargo_build_output[i];
            let (js_output, wasm_output) = self.wasm_bindgen_build(
                cbo.wasm.as_ref(), &cbo.hashed_name, cbo.is_worker,
            ).await?;
            self.wasm_opt_build(&wasm_output).await?;
            wasm_files.push(RustAppOutputWasm {
                js_output,
                wasm_output,
                is_worker: cbo.is_worker,
                worker_name: cbo.worker_name.clone(),
            });
        }
        let output = RustAppOutput {
            id: self.id,
            cfg: self.cfg.clone(),
            wasm_files,
        };

        Ok(TrunkLinkPipelineOutput::RustApp(output))
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn cargo_build(&mut self) -> Result<Vec<CargoBuildOutput>> {
        tracing::info!("building {}", &self.manifest.package.name);

        // Spawn the cargo build process.
        let mut args = vec![
            "build",
            "--target=wasm32-unknown-unknown",
            "--manifest-path",
            &self.manifest.manifest_path,
        ];
        if self.cfg.release {
            args.push("--release");
        }

        if self.worker_names.len() == 0 {
            if let Some(bin) = &self.bin {
                args.push("--bin");
                args.push(bin);
            }
        }

        if let Some(cargo_features) = &self.cargo_features {
            args.push("--features");
            args.push(cargo_features);
        }

        tracing::info!("cargo {}", args.join(" "));
        let build_res = common::run_command("cargo", Path::new("cargo"), &args)
            .await
            .context("error during cargo build execution");

        // Send cargo's target dir over to the watcher to be ignored. We must do this before
        // checking for errors, otherwise the dir will never be ignored. If we attempt to do
        // this pre-build, the canonicalization will fail and will not be ignored.
        if let Some(chan) = &mut self.ignore_chan {
            let _ = chan.try_send(self.manifest.metadata.target_directory.clone());
        }

        // Now propagate any errors which came from the cargo build.
        let _ = build_res?;

        // Perform a final cargo invocation on success to get artifact names.
        tracing::info!("fetching cargo artifacts");
        args.push("--message-format=json");
        let artifacts_out = Command::new("cargo")
            .args(args.as_slice())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("error spawning cargo build artifacts task")?
            .wait_with_output()
            .await
            .context("error getting cargo build artifacts info")?;
        if !artifacts_out.status.success() {
            eprintln!("{}", String::from_utf8_lossy(&artifacts_out.stderr));
            bail!("bad status returned from cargo artifacts request");
        }

        // Stream over cargo messages to find the artifacts we are interested in.
        let reader = std::io::BufReader::new(artifacts_out.stdout.as_slice());
        let wasm_filenames: Vec<PathBuf> = cargo_metadata::Message::parse_stream(reader)
            .filter_map(|msg| if let Ok(msg) = msg { Some(msg) } else { None })
            .filter_map(|msg| match msg {
                cargo_metadata::Message::CompilerArtifact(art) if art.package_id == self.manifest.package.id => Some(Ok(art)),
                cargo_metadata::Message::BuildFinished(finished) if !finished.success => Some(Err(anyhow!("error while fetching cargo artifact info"))),
                _ => None,
            })
            .collect::<Result<Vec<Artifact>, _>>()?
            .into_iter()
            .filter_map(|art| {
                art.filenames.into_iter().find(|path|{
                    path.extension().map(|ext| ext == "wasm").unwrap_or(false)
                })
            }).collect();

        if wasm_filenames.len() == 0 {
            bail!("could not find WASM output after cargo build");
        }

        let mut build_output: Vec<CargoBuildOutput> = match &self.bin {
            None => {
                if wasm_filenames.len() != 1 {
                    bail!("found multiple WASM output files, consider using <link> with a data-bin attribute in your index.html")
                }
                wasm_filenames.into_iter().map(|wasm| {
                    CargoBuildOutput {
                        wasm,
                        hashed_name: "".to_string(),
                        is_worker: false,
                        worker_name: None,
                    }
                }).collect()
            }
            Some(bin) => {
                let output = wasm_filenames.into_iter().filter_map(|wasm| {
                    let wasm_file_stem = wasm.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    tracing::info!("{}, {}, {}", wasm_file_stem, bin, wasm_file_stem.eq(bin));
                    if wasm_file_stem.eq(bin) {
                        return Some(CargoBuildOutput {
                            wasm,
                            hashed_name: "".to_string(),
                            is_worker: false,
                            worker_name: None,
                        });
                    }

                    if self.worker_names.iter().any(|worker_name| wasm_file_stem.eq(worker_name)) {
                        return Some(CargoBuildOutput {
                            wasm,
                            hashed_name: "".to_string(),
                            is_worker: true,
                            worker_name: Some(wasm_file_stem.clone()),
                        });
                    }
                    None
                }).collect();
                output
                // TODO check if everything has a output file
            }
        };

        tracing::info!("processing WASM");
        let hashed_name_futures: Vec<_> = build_output.iter().map(|o| {
            get_wasm_file_hashed_name(&o.wasm, &o.worker_name)
        }).collect();

        let hashed_names: Vec<String> = join_all(hashed_name_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<String>, _>>()?;

        hashed_names.into_iter().enumerate().for_each(|(i, n)| build_output[i].hashed_name = n);

        Ok(build_output)
    }

    #[tracing::instrument(level = "trace", skip(self, wasm, hashed_name))]
    async fn wasm_bindgen_build(&self, wasm: &Path, hashed_name: &str, is_worker: bool) -> Result<(String, String)> {
        let version = find_wasm_bindgen_version(&self.cfg.tools, &self.manifest);
        let wasm_bindgen = tools::get(Application::WasmBindgen, version.as_deref()).await?;

        // Ensure our output dir is in place.
        let wasm_bindgen_name = Application::WasmBindgen.name();
        let mode_segment = if self.cfg.release { "release" } else { "debug" };
        let bindgen_out = self
            .manifest
            .metadata
            .target_directory
            .join(wasm_bindgen_name)
            .join(mode_segment);
        fs::create_dir_all(bindgen_out.as_path())
            .await
            .context("error creating wasm-bindgen output dir")?;

        // Build up args for calling wasm-bindgen.
        let arg_out_path = format!("--out-dir={}", bindgen_out.display());
        let arg_out_name = format!("--out-name={}", &hashed_name);
        let arg_target = if is_worker { "--target=no-modules" } else { "--target=web" };
        let target_wasm = wasm.to_string_lossy().to_string();
        let mut args = vec![arg_target, &arg_out_path, &arg_out_name, "--no-typescript", &target_wasm];
        if self.keep_debug {
            args.push("--keep-debug");
        }
        if self.no_demangle {
            args.push("--no-demangle");
        }

        // Invoke wasm-bindgen.
        tracing::info!("calling wasm-bindgen");
        tracing::info!("{} {}", wasm_bindgen.display(), args.join(" "));
        common::run_command(wasm_bindgen_name, &wasm_bindgen, &args)
            .await
            .map_err(|err| check_target_not_found_err(err, wasm_bindgen_name))?;

        // Copy the generated WASM & JS loader to the dist dir.
        tracing::info!("copying generated wasm-bindgen artifacts");
        let hashed_js_name = format!("{}.js", &hashed_name);
        let hashed_wasm_name = format!("{}_bg.wasm", &hashed_name);
        let js_loader_path = bindgen_out.join(&hashed_js_name);
        let js_loader_path_dist = self.cfg.staging_dist.join(&hashed_js_name);
        let wasm_path = bindgen_out.join(&hashed_wasm_name);
        let wasm_path_dist = self.cfg.staging_dist.join(&hashed_wasm_name);
        fs::copy(js_loader_path, js_loader_path_dist)
            .await
            .context("error copying JS loader file to stage dir")?;
        fs::copy(wasm_path, wasm_path_dist)
            .await
            .context("error copying wasm file to stage dir")?;

        // Check for any snippets, and copy them over.
        let snippets_dir = bindgen_out.join(SNIPPETS_DIR);
        if path_exists(&snippets_dir).await? {
            copy_dir_recursive(bindgen_out.join(SNIPPETS_DIR), self.cfg.staging_dist.join(SNIPPETS_DIR))
                .await
                .context("error copying snippets dir to stage dir")?;
        }

        Ok((hashed_js_name, hashed_wasm_name))
    }

    #[tracing::instrument(level = "trace", skip(self, hashed_name))]
    async fn wasm_opt_build(&self, hashed_name: &str) -> Result<()> {
        // If not in release mode, we skip calling wasm-opt.
        if !self.cfg.release {
            return Ok(());
        }

        // If opt level is off, we skip calling wasm-opt as it wouldn't have any effect.
        if self.wasm_opt == WasmOptLevel::Off {
            return Ok(());
        }

        let version = self.cfg.tools.wasm_opt.as_deref();
        let wasm_opt = tools::get(Application::WasmOpt, version).await?;

        // Ensure our output dir is in place.
        let wasm_opt_name = Application::WasmOpt.name();
        let mode_segment = if self.cfg.release { "release" } else { "debug" };
        let output = self
            .manifest
            .metadata
            .target_directory
            .join(wasm_opt_name)
            .join(mode_segment);
        fs::create_dir_all(&output)
            .await
            .context("error creating wasm-opt output dir")?;

        // Build up args for calling wasm-opt.
        let output = output.join(hashed_name);
        let arg_output = format!("--output={}", output.display());
        let arg_opt_level = format!("-O{}", self.wasm_opt.as_ref());
        let target_wasm = self.cfg.staging_dist.join(hashed_name).to_string_lossy().to_string();
        let args = vec![&arg_output, &arg_opt_level, &target_wasm];

        // Invoke wasm-opt.
        tracing::info!("calling wasm-opt");
        common::run_command(wasm_opt_name, &wasm_opt, &args)
            .await
            .map_err(|err| check_target_not_found_err(err, wasm_opt_name))?;

        // Copy the generated WASM file to the dist dir.
        tracing::info!("copying generated wasm-opt artifacts");
        fs::copy(output, self.cfg.staging_dist.join(hashed_name))
            .await
            .context("error copying wasm file to dist dir")?;

        Ok(())
    }
}

/// Find the appropriate versio of `wasm-bindgen` to use. The version can be found in 3 different
/// location in order:
/// - Defined in the `Trunk.toml` as highest priority.
/// - Located in the `Cargo.lock` if it exists. This is mostly the case as we run `cargo build`
///   before even calling this function.
/// - Located in the `Cargo.toml` as direct dependency of the project.
fn find_wasm_bindgen_version<'a>(cfg: &'a ConfigOptsTools, manifest: &CargoMetadata) -> Option<Cow<'a, str>> {
    let find_lock = || -> Option<Cow<'_, str>> {
        let lock_path = Path::new(&manifest.manifest_path).parent()?.join("Cargo.lock");
        let lockfile = Lockfile::load(lock_path).ok()?;
        let name = "wasm-bindgen".parse().ok()?;

        lockfile
            .packages
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| Cow::from(p.version.to_string()))
    };

    let find_manifest = || -> Option<Cow<'_, str>> {
        manifest
            .metadata
            .packages
            .iter()
            .find(|p| p.name == "wasm-bindgen")
            .map(|p| Cow::from(p.version.to_string()))
    };

    cfg.wasm_bindgen
        .as_deref()
        .map(Cow::from)
        .or_else(find_lock)
        .or_else(find_manifest)
}

/// The worker specific output of a cargo build pipeline
pub struct RustAppOutputWasm {
    /// The filename of the generated JS loader file written to the dist dir.
    pub js_output: String,
    /// The filename of the generated WASM file written to the dist dir.
    pub wasm_output: String,
    /// Indicates if this is a worker.
    pub is_worker: bool,
    /// The name of the worker if this is a worker.
    pub worker_name: Option<String>,
}

/// The output of a cargo build pipeline.
pub struct RustAppOutput {
    /// The runtime build config.
    pub cfg: Arc<RtcBuild>,
    /// The ID of this pipeline.
    pub id: Option<usize>,
    /// The wasm files output by this pipeline.
    pub wasm_files: Vec<RustAppOutputWasm>,
}

impl RustAppOutput {
    pub async fn finalize(self, dom: &mut Document) -> Result<()> {
        let (base, head, body) = (&self.cfg.public_url, "html head", "html body");

        self.wasm_files.iter().for_each(|f| {
            let wasm = &f.wasm_output;
            let js = &f.js_output;
            let preload = format!(
                r#"
    <link rel="preload" href="{base}{wasm}" as="fetch" type="application/wasm" crossorigin>
    <link rel="modulepreload" href="{base}{js}">"#,
                base = base,
                js = js,
                wasm = wasm,
            );
            dom.select(head).append_html(preload);
        });

        self.wasm_files.iter().for_each(|f| {
            if !f.is_worker {
                let wasm = &f.wasm_output;
                let js = &f.js_output;
                let script = format!(
                    r#"<script type="module">import init from '{base}{js}';init('{base}{wasm}');</script>"#,
                    base = base,
                    js = js,
                    wasm = wasm,
                );
                match self.id {
                    Some(id) => dom.select(&super::trunk_id_selector(id)).replace_with_html(script),
                    None => dom.select(body).append_html(script),
                }
            }
        });
        Ok(())
    }
}

/// Different optimization levels that can be configured with `wasm-opt`.
#[derive(PartialEq, Eq)]
enum WasmOptLevel {
    /// Default optimization passes.
    Default,
    /// No optimization passes, skipping the wasp-opt step.
    Off,
    /// Run quick & useful optimizations. useful for iteration testing.
    One,
    /// Most optimizations, generally gets most performance.
    Two,
    /// Spend potentially a lot of time optimizing.
    Three,
    /// Also flatten the IR, which can take a lot more time and memory, but is useful on more nested
    /// / complex / less-optimized input.
    Four,
    /// Default optimizations, focus on code size.
    S,
    /// Default optimizations, super-focusing on code size.
    Z,
}

impl FromStr for WasmOptLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "" => Self::Default,
            "0" => Self::Off,
            "1" => Self::One,
            "2" => Self::Two,
            "3" => Self::Three,
            "4" => Self::Four,
            "s" | "S" => Self::S,
            "z" | "Z" => Self::Z,
            _ => bail!("unknown wasm-opt level `{}`", s),
        })
    }
}

impl AsRef<str> for WasmOptLevel {
    fn as_ref(&self) -> &str {
        match self {
            Self::Default => "",
            Self::Off => "0",
            Self::One => "1",
            Self::Two => "2",
            Self::Three => "3",
            Self::Four => "4",
            Self::S => "s",
            Self::Z => "z",
        }
    }
}

impl Default for WasmOptLevel {
    fn default() -> Self {
        Self::Default
    }
}

/// Handle invocation errors indicating that the target binary was not found, simply wrapping the
/// error in additional context stating more clearly that the target was not found.
fn check_target_not_found_err(err: anyhow::Error, target: &str) -> anyhow::Error {
    let io_err: &std::io::Error = match err.downcast_ref() {
        Some(io_err) => io_err,
        None => return err,
    };
    match io_err.kind() {
        std::io::ErrorKind::NotFound => err.context(format!("{} not found", target)),
        _ => err,
    }
}

async fn get_wasm_file_hashed_name(wasm: &PathBuf, worker_name: &Option<String>) -> Result<String> {
    match worker_name {
        None => {
            let wasm_bytes = fs::read(&wasm)
                .await
                .context("error reading wasm file for hash generation")?;
            let hashed_name = format!("index-{:x}", seahash::hash(&wasm_bytes));
            Ok(hashed_name)
        }
        Some(wn) => Ok(wn.clone())
    }
}