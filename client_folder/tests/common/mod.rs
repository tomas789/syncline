use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

#[allow(dead_code)]
pub struct TestCluster {
    pub server: Option<Child>,
    pub clients: Vec<(String, Child)>,
    pub base_dir: PathBuf,
    pub server_port: u16,
    pub server_db: PathBuf,
}

#[allow(dead_code)]
impl TestCluster {
    pub fn new(test_name: &str) -> Self {
        let base_dir = std::env::temp_dir().join("syncline_tests").join(test_name);
        if base_dir.exists() {
            let _ = fs::remove_dir_all(&base_dir);
        }
        fs::create_dir_all(&base_dir).unwrap();

        let server_db = base_dir.join("syncline.db");

        TestCluster {
            server: None,
            clients: Vec::new(),
            base_dir,
            server_port: 0,
            server_db,
        }
    }

    pub fn start_server(&mut self) {
        if self.server.is_some() {
            return;
        }

        // Resolve server binary relative to client_folder binary (which we know exists)
        let client_bin = env!("CARGO_BIN_EXE_client_folder");
        let bin_path_buf = std::path::Path::new(client_bin)
            .parent()
            .unwrap()
            .join("server");
        let bin_path = bin_path_buf.to_str().unwrap();

        let port_arg = if self.server_port == 0 {
            "0".to_string()
        } else {
            self.server_port.to_string()
        };

        let mut child = Command::new(bin_path)
            .arg("--port")
            .arg(&port_arg)
            .arg("--db-path")
            .arg(self.server_db.to_str().unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start server");

        let stdout = child.stdout.take().expect("Failed to open server stdout");
        let reader = BufReader::new(stdout);

        // We need updates on the port if we passed 0
        let (tx, rx) = std::sync::mpsc::channel();
        let must_parse = self.server_port == 0;

        thread::spawn(move || {
            for line in reader.lines().flatten() {
                println!("Server: {}", line);
                if line.contains("Server listening on") && must_parse {
                    if let Some(pos) = line.rfind(':') {
                        if let Ok(port) = line[pos + 1..].trim().parse::<u16>() {
                            let _ = tx.send(port);
                        }
                    }
                }
            }
        });

        if must_parse {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(port) => self.server_port = port,
                Err(_) => panic!("Server failed to start or print port"),
            }
        } else {
            // Just give it a moment to bind
            thread::sleep(Duration::from_millis(500));
        }

        self.server = Some(child);
        println!("Test Server started on port {}", self.server_port);
    }

    pub fn stop_server(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn start_client(&mut self, name: &str) {
        let client_dir = self.base_dir.join(name);
        if !client_dir.exists() {
            fs::create_dir_all(&client_dir).unwrap();
        }

        let bin_path = env!("CARGO_BIN_EXE_client_folder");
        let url = format!("ws://127.0.0.1:{}", self.server_port);

        // We use client_folder here as it's the main sync agent
        let mut child = Command::new(bin_path)
            .arg("--url")
            .arg(url)
            .arg("--dir")
            .arg(client_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Failed to start client");

        let stdout = child.stdout.take().expect("Failed to open client stdout");
        let reader = BufReader::new(stdout);
        let name_clone = name.to_string();
        thread::spawn(move || {
            for line in reader.lines().flatten() {
                println!("Client({}): {}", name_clone, line);
            }
        });

        self.clients.push((name.to_string(), child));
        // Wait for client to connect
        thread::sleep(Duration::from_millis(1000));
    }

    #[allow(dead_code)]
    pub fn stop_client(&mut self, name: &str) {
        if let Some(pos) = self.clients.iter().position(|(n, _)| n == name) {
            let (_, mut child) = self.clients.remove(pos);
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn get_client_dir(&self, name: &str) -> PathBuf {
        self.base_dir.join(name)
    }

    pub fn write_file(&self, client_name: &str, file_name: &str, content: &str) {
        let path = self.get_client_dir(client_name).join(file_name);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, content).unwrap();
    }

    pub fn read_file(&self, client_name: &str, file_name: &str) -> String {
        let path = self.get_client_dir(client_name).join(file_name);
        fs::read_to_string(path).unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn write_binary_file(&self, client_name: &str, file_name: &str, content: &[u8]) {
        let path = self.get_client_dir(client_name).join(file_name);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, content).unwrap();
    }

    #[allow(dead_code)]
    pub fn read_binary_file(&self, client_name: &str, file_name: &str) -> Vec<u8> {
        let path = self.get_client_dir(client_name).join(file_name);
        fs::read(path).unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn delete_file(&self, client_name: &str, file_name: &str) {
        let path = self.get_client_dir(client_name).join(file_name);
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }

    #[allow(dead_code)]
    pub fn rename_file(&self, client_name: &str, old: &str, new: &str) {
        let old_path = self.get_client_dir(client_name).join(old);
        let new_path = self.get_client_dir(client_name).join(new);
        fs::rename(old_path, new_path).unwrap();
    }

    pub fn file_exists(&self, client_name: &str, file_name: &str) -> bool {
        self.get_client_dir(client_name).join(file_name).exists()
    }

    pub fn wait_sync(&self) {
        thread::sleep(Duration::from_millis(2000)); // Increased wait time for safety
    }
}

impl Drop for TestCluster {
    fn drop(&mut self) {
        self.stop_server();
        for (_, mut child) in self.clients.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
