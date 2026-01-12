use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServiceType {
    NpmScript {
        script_name: String,
        package_json_path: PathBuf,
    },
    DockerCompose {
        service_name: String,
        compose_file_path: PathBuf,
    },
    Procfile {
        process_name: String,
        procfile_path: PathBuf,
    },
    PythonApp {
        script_path: PathBuf,
    },
    Custom {
        command: Vec<String>,
        working_dir: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredService {
    pub name: String,
    pub service_type: ServiceType,
    pub working_directory: PathBuf,
    pub inferred_port: Option<u16>,
    pub description: String,
}

pub struct ServiceDetector {
    search_paths: Vec<PathBuf>,
}

impl ServiceDetector {
    pub fn new() -> Self {
        Self {
            search_paths: vec![PathBuf::from(".")],
        }
    }

    pub fn with_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            search_paths: paths,
        }
    }

    /// Discover all services in the search paths
    pub fn discover_services(&self) -> Result<Vec<DiscoveredService>> {
        let mut services = Vec::new();

        for search_path in &self.search_paths {
            // Search for package.json (Node.js/npm)
            services.extend(self.discover_npm_services(search_path)?);

            // Search for docker-compose.yml
            services.extend(self.discover_docker_services(search_path)?);

            // Search for Procfile
            services.extend(self.discover_procfile_services(search_path)?);

            // Search for Python apps
            services.extend(self.discover_python_services(search_path)?);
        }

        Ok(services)
    }

    /// Start a discovered service
    /// Returns the PID of the spawned process. The child process is detached
    /// to prevent zombie process accumulation.
    pub fn start_service(&self, service: &DiscoveredService) -> Result<u32> {
        log::info!("Starting service: {}", service.name);

        let mut child = match &service.service_type {
            ServiceType::NpmScript {
                script_name,
                package_json_path,
            } => self.start_npm_script(script_name, package_json_path),

            ServiceType::DockerCompose {
                service_name,
                compose_file_path,
            } => self.start_docker_service(service_name, compose_file_path),

            ServiceType::Procfile {
                process_name,
                procfile_path,
            } => self.start_procfile_process(process_name, procfile_path),

            ServiceType::PythonApp { script_path } => self.start_python_app(script_path),

            ServiceType::Custom {
                command,
                working_dir,
            } => self.start_custom_command(command, working_dir),
        }?;
        
        let pid = child.id();

        // Spawn a background thread to reap the child when it exits,
        // preventing zombie process accumulation
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        Ok(pid)
    }

    // Private methods for discovering services

    fn discover_npm_services(&self, search_path: &Path) -> Result<Vec<DiscoveredService>> {
        let mut services = Vec::new();
        let package_json_path = search_path.join("package.json");

        if !package_json_path.exists() {
            return Ok(services);
        }

        let content = fs::read_to_string(&package_json_path)
            .context("Failed to read package.json")?;
        
        let package_json: serde_json::Value = serde_json::from_str(&content)
            .context("Failed to parse package.json")?;

        // Extract scripts
        if let Some(scripts) = package_json.get("scripts").and_then(|s| s.as_object()) {
            for (script_name, _script_value) in scripts {
                // Skip test and build scripts
                if script_name.contains("test") || script_name.contains("build") {
                    continue;
                }

                let inferred_port = Self::infer_port_from_script_name(script_name);
                
                services.push(DiscoveredService {
                    name: format!("npm:{}", script_name),
                    service_type: ServiceType::NpmScript {
                        script_name: script_name.clone(),
                        package_json_path: package_json_path.clone(),
                    },
                    working_directory: search_path.to_path_buf(),
                    inferred_port,
                    description: format!("npm run {} (Node.js)", script_name),
                });
            }
        }

        Ok(services)
    }

    fn discover_docker_services(&self, search_path: &Path) -> Result<Vec<DiscoveredService>> {
        let mut services = Vec::new();
        
        let compose_files = ["docker-compose.yml", "docker-compose.yaml", "compose.yml"];
        
        for compose_file_name in &compose_files {
            let compose_path = search_path.join(compose_file_name);
            
            if !compose_path.exists() {
                continue;
            }

            let content = fs::read_to_string(&compose_path)
                .context("Failed to read docker-compose file")?;

            // Simple YAML parsing for service names (proper YAML parser would be better)
            for line in content.lines() {
                if let Some(service_name) = Self::extract_docker_service_name(line) {
                    services.push(DiscoveredService {
                        name: format!("docker:{}", service_name),
                        service_type: ServiceType::DockerCompose {
                            service_name: service_name.clone(),
                            compose_file_path: compose_path.clone(),
                        },
                        working_directory: search_path.to_path_buf(),
                        inferred_port: None,
                        description: format!("Docker Compose service: {}", service_name),
                    });
                }
            }
        }

        Ok(services)
    }

    fn discover_procfile_services(&self, search_path: &Path) -> Result<Vec<DiscoveredService>> {
        let mut services = Vec::new();
        let procfile_path = search_path.join("Procfile");

        if !procfile_path.exists() {
            return Ok(services);
        }

        let content = fs::read_to_string(&procfile_path)
            .context("Failed to read Procfile")?;

        for line in content.lines() {
            if let Some((process_name, _command)) = Self::parse_procfile_line(line) {
                services.push(DiscoveredService {
                    name: format!("procfile:{}", process_name),
                    service_type: ServiceType::Procfile {
                        process_name: process_name.clone(),
                        procfile_path: procfile_path.clone(),
                    },
                    working_directory: search_path.to_path_buf(),
                    inferred_port: None,
                    description: format!("Procfile process: {}", process_name),
                });
            }
        }

        Ok(services)
    }

    fn discover_python_services(&self, search_path: &Path) -> Result<Vec<DiscoveredService>> {
        let mut services = Vec::new();

        // Look for common Python app files
        let python_files = ["app.py", "main.py", "manage.py", "run.py"];

        for py_file in &python_files {
            let py_path = search_path.join(py_file);
            
            if py_path.exists() {
                let inferred_port = if *py_file == "manage.py" {
                    Some(8000) // Django default
                } else {
                    Some(5000) // Flask default
                };

                services.push(DiscoveredService {
                    name: format!("python:{}", py_file),
                    service_type: ServiceType::PythonApp {
                        script_path: py_path.clone(),
                    },
                    working_directory: search_path.to_path_buf(),
                    inferred_port,
                    description: format!("Python app: {}", py_file),
                });
            }
        }

        Ok(services)
    }

    // Private methods for starting services

    fn start_npm_script(&self, script_name: &str, package_json_path: &Path) -> Result<Child> {
        let working_dir = package_json_path.parent().unwrap_or(Path::new("."));

        Command::new("npm")
            .arg("run")
            .arg(script_name)
            .current_dir(working_dir)
            .spawn()
            .context(format!("Failed to start npm script: {}", script_name))
    }

    fn start_docker_service(&self, service_name: &str, compose_file_path: &Path) -> Result<Child> {
        let working_dir = compose_file_path.parent().unwrap_or(Path::new("."));

        Command::new("docker-compose")
            .arg("up")
            .arg(service_name)
            .current_dir(working_dir)
            .spawn()
            .context(format!("Failed to start docker service: {}", service_name))
    }

    fn start_procfile_process(&self, process_name: &str, procfile_path: &Path) -> Result<Child> {
        let working_dir = procfile_path.parent().unwrap_or(Path::new("."));
        
        // Read the Procfile to get the command
        let content = fs::read_to_string(procfile_path)
            .context("Failed to read Procfile")?;

        for line in content.lines() {
            if let Some((name, command)) = Self::parse_procfile_line(line) {
                if name == process_name {
                    return self.execute_shell_command(&command, working_dir);
                }
            }
        }

        Err(anyhow::anyhow!("Process {} not found in Procfile", process_name))
    }

    fn start_python_app(&self, script_path: &Path) -> Result<Child> {
        let working_dir = script_path.parent().unwrap_or(Path::new("."));
        let script_name = script_path.file_name().unwrap().to_str().unwrap();

        let command = if script_name == "manage.py" {
            vec!["python", script_name, "runserver"]
        } else {
            vec!["python", script_name]
        };

        Command::new(command[0])
            .args(&command[1..])
            .current_dir(working_dir)
            .spawn()
            .context(format!("Failed to start Python app: {}", script_name))
    }

    fn start_custom_command(&self, command: &[String], working_dir: &Path) -> Result<Child> {
        if command.is_empty() {
            return Err(anyhow::anyhow!("Empty command"));
        }

        Command::new(&command[0])
            .args(&command[1..])
            .current_dir(working_dir)
            .spawn()
            .context("Failed to start custom command")
    }

    fn execute_shell_command(&self, command: &str, working_dir: &Path) -> Result<Child> {
        #[cfg(target_os = "windows")]
        {
            Command::new("cmd")
                .arg("/C")
                .arg(command)
                .current_dir(working_dir)
                .spawn()
                .context("Failed to execute shell command")
        }

        #[cfg(not(target_os = "windows"))]
        {
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(working_dir)
                .spawn()
                .context("Failed to execute shell command")
        }
    }

    // Helper methods

    fn infer_port_from_script_name(script_name: &str) -> Option<u16> {
        // Try to infer port from common script names
        match script_name {
            "dev" | "start" | "serve" => Some(3000), // React/Next.js default
            "dev:api" | "start:api" => Some(8000),
            "dev:frontend" => Some(3000),
            "dev:backend" => Some(8000),
            _ => None,
        }
    }

    fn extract_docker_service_name(line: &str) -> Option<String> {
        // Extract service names from docker-compose.yml by checking indentation
        // Service names appear exactly 2 spaces indented under "services:"
        // Format: "  service_name:" (2 spaces, then name, then colon)
        // Properties like ports:, environment:, volumes: have 4+ spaces

        // Check for exactly 2 spaces of indentation (service level)
        // This filters out top-level keys (0 spaces) and properties (4+ spaces)
        if !line.starts_with("  ") || line.starts_with("   ") {
            return None;
        }

        let trimmed = line.trim();
        
        // Skip the services: key itself and comments
        if trimmed.starts_with("services:") || trimmed.starts_with('#') {
            return None;
        }
        
        if trimmed.ends_with(':') {
            let name = trimmed.trim_end_matches(':').trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                return Some(name.to_string());
            }
        }
        
        None
    }

    fn parse_procfile_line(line: &str) -> Option<(String, String)> {
        // Format: "process_name: command"
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }

        if let Some(colon_pos) = trimmed.find(':') {
            let process_name = trimmed[..colon_pos].trim().to_string();
            let command = trimmed[colon_pos + 1..].trim().to_string();
            
            if !process_name.is_empty() && !command.is_empty() {
                return Some((process_name, command));
            }
        }

        None
    }
}

impl Default for ServiceDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_procfile_line() {
        let line = "web: npm start";
        let result = ServiceDetector::parse_procfile_line(line);
        assert_eq!(result, Some(("web".to_string(), "npm start".to_string())));
    }

    #[test]
    fn test_extract_docker_service_name() {
        let line = "  web:";
        let result = ServiceDetector::extract_docker_service_name(line);
        assert_eq!(result, Some("web".to_string()));
    }

    #[test]
    fn test_infer_port() {
        assert_eq!(ServiceDetector::infer_port_from_script_name("dev"), Some(3000));
        assert_eq!(ServiceDetector::infer_port_from_script_name("dev:api"), Some(8000));
    }
}

