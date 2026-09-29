//! `sabre-server` — run the tile server as a plain binary.

mod cli {
    use std::path::PathBuf;
    use std::time::Duration;

    use sabre_server::native::{NativeBackend, NativeConfig};

    const USAGE: &str = "\
sabre-server — tile server for Cloud-Optimized GeoTIFFs

USAGE:
    sabre-server [OPTIONS]

OPTIONS:
    --bind <ADDR>        Address to listen on        [env: SABRE_BIND]      [default: 127.0.0.1:8787]
    --file-root <DIR>    Enable file:// sources, resolved inside DIR
                                                     [env: SABRE_FILE_ROOT]
    --threads <N>        Request threads             [env: SABRE_THREADS]   [default: number of CPUs]
    --cache-size <SIZE>  Memory for cached source data, headers included; 0 disables the cache
                         (e.g. 512M, 2G)              [env: SABRE_CACHE_SIZE] [default: 256M]
    --cache-ttl <SECS>   How long cached data stays valid; 0 keeps it until evicted
                                                     [env: SABRE_CACHE_TTL] [default: 3600]
    --geometry-cache-size <SIZE>
                         Memory for resolved geometry and access decisions. A separate
                         pool from --cache-size; the two add up (e.g. 64M, 0 disables)
                                                     [env: SABRE_GEOMETRY_CACHE_SIZE]
                                                     [default: 64M]
    --geometry-providers <SPEC|@FILE>
                         Upstreams that geometry_provider may name, one per line:
                             blocks=https://api.example.com/blocks/{id}/geometry
                             farms=https://internal/geom/{id} access=https://internal/ok?ids={ids}
                         Options after the URL: access, auth=yes|no, timeout=<ms>,
                         max-bytes=<size>, geometry-ttl=<s>, access-ttl=<s>.
                         An @-prefixed value is read from that file.
                                                     [env: SABRE_GEOMETRY_PROVIDERS]
    -h, --help           Print this help
";

    struct Args {
        bind:   String,
        config: NativeConfig,
    }

    fn parse_args() -> Result<Args, String> {
        let mut bind      = std::env::var("SABRE_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
        let mut file_root = std::env::var_os("SABRE_FILE_ROOT").map(PathBuf::from);
        let mut threads   = std::env::var("SABRE_THREADS").ok();
        let mut cache_size = std::env::var("SABRE_CACHE_SIZE").ok();
        let mut cache_ttl  = std::env::var("SABRE_CACHE_TTL").ok();
        let mut providers  = std::env::var("SABRE_GEOMETRY_PROVIDERS").ok();
        let mut geom_cache = std::env::var("SABRE_GEOMETRY_CACHE_SIZE").ok();

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} requires a value"));
            match arg.as_str() {
                "--bind"      => bind      = value("--bind")?,
                "--file-root" => file_root = Some(PathBuf::from(value("--file-root")?)),
                "--threads"   => threads   = Some(value("--threads")?),
                "--cache-size" => cache_size = Some(value("--cache-size")?),
                "--cache-ttl"  => cache_ttl  = Some(value("--cache-ttl")?),
                "--geometry-providers" => providers = Some(value("--geometry-providers")?),
                "--geometry-cache-size" => geom_cache = Some(value("--geometry-cache-size")?),
                "-h" | "--help" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
            }
        }

        let mut config = NativeConfig { file_root, ..NativeConfig::default() };
        if let Some(n) = threads {
            config.threads = n.parse().map_err(|_| format!("--threads: expected a number, got {n:?}"))?;
        }
        if let Some(size) = cache_size {
            config.cache_bytes = parse_size(&size).ok_or_else(|| format!("--cache-size: expected a size like 256M, got {size:?}"))?;
        }
        if let Some(secs) = cache_ttl {
            let secs: u64 = secs.parse().map_err(|_| format!("--cache-ttl: expected a number of seconds, got {secs:?}"))?;
            config.cache_ttl = (secs > 0).then(|| Duration::from_secs(secs));
        }
        if let Some(size) = geom_cache {
            config.geometry_cache_bytes = parse_size(&size)
                .ok_or_else(|| format!("--geometry-cache-size: expected a size like 64M, got {size:?}"))?;
        }
        if let Some(spec) = providers {
            // A file, because a provider list with credentials-adjacent URLs
            // in it does not belong in a process listing.
            let spec = match spec.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path)
                    .map_err(|e| format!("--geometry-providers @{path}: {e}"))?,
                None => spec,
            };
            config.providers = sabre_server::geometry::Registry::parse(&spec)?;
        }
        Ok(Args { bind, config })
    }

    /// A byte count with an optional K, M or G suffix (powers of 1024): `0`,
    /// `1048576`, `256M`, `2g`.
    pub fn parse_size(text: &str) -> Option<usize> {
        let text = text.trim();
        let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
            Some((i, _)) => text.split_at(i),
            None         => (text, ""),
        };
        let shift = match unit.trim().to_ascii_uppercase().as_str() {
            "" | "B" => 0,
            "K" | "KB" => 10,
            "M" | "MB" => 20,
            "G" | "GB" => 30,
            _ => return None,
        };
        digits.parse::<usize>().ok()?.checked_shl(shift)
    }

    pub async fn run() -> Result<(), String> {
        let args = parse_args()?;
        let backend = NativeBackend::new(args.config.clone())?;
        let listener = tokio::net::TcpListener::bind(&args.bind).await
            .map_err(|e| format!("cannot bind {}: {e}", args.bind))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;

        eprintln!("sabre-server listening on http://{addr}");
        match &args.config.file_root {
            Some(root) => eprintln!("file:// sources enabled under {}", root.display()),
            None       => eprintln!("file:// sources disabled (pass --file-root to enable)"),
        }
        match args.config.providers.names().as_slice() {
            []    => eprintln!("no geometry providers (pass --geometry-providers to enable geometry_provider)"),
            names => {
                eprintln!("geometry providers: {}", names.join(", "));
                // Said out loud because it is a second pool: someone who set
                // --cache-size 256M has 320 MB of cache, not 256.
                match args.config.geometry_cache_bytes {
                    0 => eprintln!("geometry cache disabled"),
                    b => eprintln!("geometry cache: {} MB", b >> 20),
                }
            }
        }
        match (args.config.cache_bytes, args.config.cache_ttl) {
            (0, _)          => eprintln!("source cache disabled"),
            (bytes, None)   => eprintln!("source cache: {} MB, no expiry", bytes >> 20),
            (bytes, Some(t)) => eprintln!("source cache: {} MB, {} s TTL", bytes >> 20, t.as_secs()),
        }

        axum::serve(listener, sabre_server::router(backend))
            .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.ok(); })
            .await
            .map_err(|e| e.to_string())
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = cli::run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::cli::parse_size;

    #[test]
    fn sizes_take_an_optional_unit() {
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("1048576"), Some(1 << 20));
        assert_eq!(parse_size("256M"), Some(256 << 20));
        assert_eq!(parse_size(" 2g "), Some(2 << 30));
        assert_eq!(parse_size("64 KB"), Some(64 << 10));
        assert_eq!(parse_size("1T"), None);
        assert_eq!(parse_size("M"), None);
        assert_eq!(parse_size(""), None);
    }
}
