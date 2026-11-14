// 签名 catalog 下发消费端（docs/16 P3/P6/P7/P8）。decode → 验签 → acceptance → 持久化语义均
// 经单测；持久化是真实 SQLite（`SqliteCatalogDeliveryState`），文件源 `FileCatalogFetcher` +
// `CatalogDelivery` 门面把零件装成可用服务，HTTP fetcher 的请求成形/错误映射（`HttpCatalogFetcher`
// over `CatalogHttpTransport`）与其 reqwest 实现（`ReqwestCatalogHttpTransport`，blocking +
// https 强制 + 有界读）也已就位。`catalog_sync` 在 Tauri async 循环中用 `spawn_blocking`
// 完成开机/定时 refresh，并只把重新验签的缓存交给运行时 WindowTracker。
#[cfg(not(feature = "local-catalog-only"))]
pub mod api_endpoint;
#[allow(dead_code)]
pub mod catalog_delivery;
#[cfg(not(feature = "local-catalog-only"))]
pub mod catalog_sync;
pub mod client_events;
pub mod consent_watch;
#[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
pub mod cp2077_trial;
// 标注员账号（docs/11 §7）。编排逻辑（状态转移、凭据落盘、错误映射）经单测；
// 命令层、UI 与设备 enroll 快照均从这里接入；账号面仍不作为采集前置。
pub mod account;
pub mod capture_storage;
pub mod credentials;
pub mod data_model;
// 深度采集（实验，docs/77 §6 / architecture §6.6）。只在 depth-lab feature 下编译；
// 门禁、版本化 probe 生命周期与 Flushed-only finalize 证明均在共享契约中严格校验。
#[cfg(feature = "depth-lab")]
#[allow(dead_code)] // U2 核心完成；worker 唤醒与短期 depth bearer 注入由后续生命周期接线。
pub mod depth_archive;
#[cfg(feature = "depth-lab")]
#[allow(dead_code)] // 独立 depth-ingest HTTP adapter；不接 production uploader。
pub mod depth_archive_http;
#[cfg(feature = "depth-lab")]
pub mod depth_certification_delivery;
#[cfg(feature = "depth-lab")]
pub mod depth_finalize;
#[cfg(feature = "depth-lab")]
#[allow(dead_code)] // CA-3 wires lifecycle; CA-4/finalization consumers remain optional surfaces.
pub mod depth_lab;
pub mod diagnostics;
#[cfg(feature = "depth-lab")]
mod early_launch;
#[cfg(not(feature = "local-catalog-only"))]
pub mod game_admission;
pub mod heartbeats;
pub mod journal;
pub mod lifecycle;
pub mod markers;
// 录制期设置文件监视（docs/45/46，C4）：变更即 SettingsChanged 停录并失效标定。
pub mod settings_watch;
// 视角标定服务（docs/45/46，C3）：对表状态机 + 证据落盘 + journal 登记。
pub mod view_scale;
// HTTP endpoint / uploader wiring 尚未落地；当前 PR 交付的是经过单测的 durable
// enqueue + drain 语义。保留模块编译检查，但在 wiring 接入前不伪造生产调用点。
#[allow(dead_code)]
pub mod mirror;
pub mod orchestrator;
pub mod overlay;
pub mod preflight;
pub mod recovery;
mod run_directory;
pub mod sentry;
pub mod session_artifacts;
pub mod session_history;
pub mod storage;
pub mod updater;
pub mod uploader;

#[cfg(test)]
pub(crate) mod test_http {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    pub struct RedirectProbe {
        pub source_url: String,
        target_received: Receiver<bool>,
        source: JoinHandle<()>,
        target: JoinHandle<()>,
    }

    impl RedirectProbe {
        pub fn finish(self) -> bool {
            let received = self
                .target_received
                .recv_timeout(Duration::from_secs(3))
                .expect("redirect target probe did not finish");
            self.source.join().expect("redirect source server panicked");
            self.target.join().expect("redirect target server panicked");
            received
        }
    }

    pub fn redirect_probe() -> RedirectProbe {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_address = target.local_addr().unwrap();
        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source.local_addr().unwrap();
        let (target_received_tx, target_received) = mpsc::channel();

        let target_handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match target.accept() {
                    Ok((mut stream, _)) => {
                        read_request(&mut stream);
                        target_received_tx.send(true).unwrap();
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 31\r\nConnection: close\r\n\r\n{\"policyVersion\":\"redirected\"}\n",
                            )
                            .unwrap();
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            target_received_tx.send(false).unwrap();
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("redirect target accept failed: {error}"),
                }
            }
        });

        let source_handle = thread::spawn(move || {
            let (mut stream, _) = source.accept().unwrap();
            read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target_address}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        RedirectProbe {
            source_url: format!("http://{source_address}/"),
            target_received,
            source: source_handle,
            target: target_handle,
        }
    }

    fn read_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let (header_bytes, content_length) = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "client closed before sending request headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let header_bytes = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                break (header_bytes, content_length);
            }
        };
        while request.len() < header_bytes + content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "client closed before sending request body");
            request.extend_from_slice(&buffer[..read]);
        }
    }
}
