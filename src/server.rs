use crate::{
    config::{Config, ARGS, CONFIG, CONFIG_PATH, DEFAULT_CONFIG, DEFAULT_INTERVAL},
    init::{init_cache, init_signal, DATE_FORMAT, FILE_CACHE, INDEX_CACHE, T},
    route::{location_index, mime_match, root_relative, status_page},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use mime::Mime;
use std::{
    collections::HashMap, env::set_current_dir, error::Error, ops::Deref, path::Path, sync::Arc,
};

#[cfg(feature = "log")]
use {crate::init::init_logger, log::logger};

#[cfg(target_os = "android")]
use std::os::android::fs::MetadataExt;

#[cfg(target_os = "linux")]
use std::os::linux::fs::MetadataExt;

use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::Semaphore,
    time::timeout,
};

#[derive(Clone)]
struct Response<'a> {
    version: &'a str,
    status_code: i32,
    _headers_buffer: HashMap<&'a str, String>,
}

impl<'a> Response<'a> {
    #[inline]
    fn send_header<T>(&mut self, k: &'a str, v: T) -> Option<String>
    where
        T: ToString,
    {
        self._headers_buffer.insert(k, v.to_string())
    }
    #[inline]
    fn resp(&mut self) -> String {
        let (version, status_code) = (self.version, self.status_code);
        let mut resp = format!("HTTP/{} {}\r\n", version, self.status(status_code));
        for (key, value) in &self._headers_buffer {
            resp.push_str(&format!("{}: {}\r\n", key, value));
        }
        resp.push_str("\r\n");
        resp
    }
    #[inline]
    fn status(&mut self, status_code: i32) -> String {
        let status = match status_code {
            200 => "OK",
            301 => "Moved Permanently",
            400 => "Bad Request",
            404 => "Not Found",
            501 => "Not Implemented",
            _ => "Internal Server Error", // 500
        };

        format!("{} {}", status_code, status)
    }
}

async fn handle_connection<S>(mut stream: S) -> Result<(i32, String)>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let config = CONFIG.load();
    let cache_config = config.server.cache.clone().unwrap_or_default();

    let mut response: Response = Response {
        version: "1.1",
        status_code: 200,
        _headers_buffer: HashMap::new(),
    };

    let server_info = format!(
        "Zest/{} ({})",
        env!("CARGO_PKG_VERSION"),
        config.server.info
    );
    response.send_header("Server", server_info.clone());

    response.send_header("Date", Utc::now().format(DATE_FORMAT));

    let buf_reader = BufReader::new(&mut stream);
    let req = buf_reader.lines().next_line().await?.unwrap_or_default();

    // GET /location HTTP/1.1
    let parts: Vec<&str> = req.split('/').collect();

    let mut mime_type: Mime = mime::TEXT_HTML_UTF_8;
    let mut buffer: Vec<u8> = Vec::new();

    if parts.len() < 3 {
        response.status_code = 400;
    } else if parts.first().unwrap().trim() != "GET" {
        response.status_code = 501;
    } else if let Some(location) = &req.split_whitespace().nth(1) {
        let location: String = urlencoding::decode(root_relative(location))
            .unwrap_or_default()
            .into();

        response.version = parts.last().unwrap();
        let mut path = config.server.root.join(location.split('?').next().unwrap());

        path = match path.canonicalize() {
            Ok(canonical_path) => canonical_path,
            Err(_) => {
                response.status_code = 404;
                config
                    .server
                    .root
                    .join(Path::new(
                        &config
                            .server
                            .error_page
                            .clone()
                            .unwrap_or("404.html".into()),
                    ))
                    .to_path_buf()
                    .canonicalize()
                    .unwrap_or_default()
            }
        };
        if path.is_dir() {
            #[allow(unused_assignments)]
            let mut html: String = String::new();
            #[cfg(feature = "lru_cache")]
            {
                let mut cache = INDEX_CACHE.write().await;
                if let Some(ctx) = cache.get(&location) {
                    html.clone_from(ctx);
                } else if let Ok(index) = location_index(path, &location).await {
                    cache
                        .push(location.clone(), index)
                        .to_owned()
                        .unwrap_or_default();

                    html.clone_from(cache.get(&location).unwrap());
                } else {
                    response.status_code = 301;
                }
            }
            #[cfg(not(feature = "lru_cache"))]
            {
                if let Ok(index) = location_index(path, &location).await {
                    html = index;
                } else {
                    response.status_code = 301;
                }
            }

            buffer = html.into_bytes();
        } else {
            // path.is_file()
            match File::open(path.clone()).await {
                Ok(f) => {
                    let mut file = f;
                    mime_type = mime_match(path.to_str().unwrap());

                    #[cfg(feature = "lru_cache")]
                    {
                        let mut cache = FILE_CACHE.write().await;
                        if let Some(content) = cache.get(&location) {
                            buffer = content.to_vec();
                        } else {
                            file.read_to_end(&mut buffer).await?;
                            if file.metadata().await.unwrap().len()
                                < cache_config
                                    .file_maxsize
                                    .unwrap_or(32768 * 1024 /* 32 MB */)
                            {
                                cache
                                    .push(location.clone(), buffer.clone())
                                    .to_owned()
                                    .unwrap_or_default();
                            }
                        }
                    }

                    #[cfg(not(feature = "lru_cache"))]
                    file.read_to_end(&mut buffer).await?;

                    response.send_header(
                        "Last-Modified",
                        DateTime::from_timestamp(file.metadata().await?.st_atime(), 0)
                            .unwrap()
                            .format(DATE_FORMAT),
                    );
                }
                Err(_) => {
                    response.status_code = 500;
                }
            };
        }
    } else {
        response.status_code = 400;
    }

    if response.status_code != 200 {
        buffer = status_page(&response.status(response.status_code), server_info)
            .await
            .into()
    }
    response.send_header("Content-Length", buffer.len());
    response.send_header("Content-Type", mime_type);
    stream.write_all(response.resp().as_bytes()).await?;
    stream.write_all(&buffer).await?;
    stream.flush().await?;
    stream.shutdown().await?;

    Ok((response.status_code, req))
}

pub async fn zest_listener<C>(config: C) -> Result<(), Box<dyn Error>>
where
    C: Deref<Target = Config>,
{
    let listener =
        TcpListener::bind(format!("{}:{}", config.bind.addr, config.bind.listen)).await?;

    let mut _allowlist: Option<Vec<String>> = config.clone().allowlist;
    let mut _blocklist: Option<Vec<String>> = config.clone().blocklist;

    let rate_limiter = Arc::new(if let Some(rate_limit) = &config.rate_limit {
        Semaphore::new(rate_limit.max_requests)
    } else {
        Semaphore::new(Semaphore::MAX_PERMITS)
    });

    #[allow(unused_labels)]
    'handle: loop {
        if T.read().await.is_none() {
            #[cfg(feature = "log")]
            info!("config reloaded!");

            return Ok(());
        }
        #[allow(unused_mut)]
        if let Ok(Ok((mut stream, _addr))) = timeout(
            config.server.interval.unwrap_or(*DEFAULT_INTERVAL),
            listener.accept(),
        )
        .await
        {
            #[cfg(feature = "ip_limit")]
            {
                if let Some(ref allowlist) = _allowlist {
                    for item in allowlist {
                        if let Ok(cidr) = item.parse::<ipnet::IpNet>() {
                            if !cidr.contains(&_addr.ip()) {
                                if allowlist.last() != Some(item) {
                                    continue;
                                } else {
                                    stream.shutdown().await?;
                                    continue 'handle;
                                }
                            }
                        }
                    }
                }

                if let Some(ref blocklist) = _blocklist {
                    for item in blocklist {
                        if let Ok(cidr) = item.parse::<ipnet::IpNet>() {
                            if cidr.contains(&_addr.ip()) {
                                stream.shutdown().await?;
                                continue 'handle;
                            }
                        }
                    }
                }
            }

            let rate_limiter = Arc::clone(&rate_limiter);
            tokio::spawn(async move {
                if rate_limiter.clone().try_acquire_owned().is_ok() {
                    let (_status_code, _req) = handle_connection(stream).await.unwrap_or_default();

                    #[cfg(feature = "log")]
                    {
                        match _status_code {
                            200 => {
                                info!("\"{}\" {} - {}", _req, _status_code, _addr);
                            }
                            400.. => {
                                error!("\"{}\" {} - {}", _req, _status_code, _addr);
                            }
                            _ => {
                                warn!("\"{}\" {} - {}", _req, _status_code, _addr);
                            }
                        };
                    }
                } else {
                    let _ = stream.shutdown().await;
                }
            });
        }
    }
}

pub async fn zest_main() -> Result<(), Box<dyn Error>> {
    *CONFIG_PATH.lock()? = ARGS.config.clone().unwrap_or_default();
    let config = DEFAULT_CONFIG.deref();

    set_current_dir(config.clone().server.root)?;

    #[cfg(feature = "log")]
    init_logger(&config.clone())
        .await
        .context("failed to init logger")?;

    init_signal().await.context("failed to init signal")?;

    #[cfg(feature = "lru_cache")]
    init_cache().await.context("failed to init lru cache")?;

    loop {
        let config = CONFIG.load();

        let handle = tokio::spawn(async move {
            zest_listener(config.clone()).await.unwrap();
        });

        let mut t = T.try_write().unwrap();
        *t = Some(0);
        drop(t);

        let _ = handle.await;
    }
}
