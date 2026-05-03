# Lotus GPU Miner

A high-performance GPU mining software for the [Lotus Network](https://lotusia.org/), implemented in Rust with OpenCL and Metal support. This miner enables you to participate in Lotus's Proof-of-Work consensus mechanism using your GPU's computational power.

## Features

- **GPU Mining**: OpenCL-based mining kernels for AMD and NVIDIA GPUs (Linux/Windows), Metal compute shaders for Apple Silicon (macOS)
- **Dual Operation Modes**:
  - **RPC Mode**: Connect directly to a Lotus node via RPC
  - **Stratum Mode**: Connect to a stratum pool server for pooled mining
- **Cross-Platform**: Works on Windows, macOS, and Linux
- **CLI and GUI**: Command-line interface for advanced users, graphical interface for ease of use
- **Real-time Metrics**: Live hashrate monitoring and logging
- **Configurable**: Extensive configuration options for kernel size, GPU selection, and mining parameters

## Project Structure

```
lotus-gpu-miner/
├── kernels/              # Mining kernels
│   ├── lotus_og.cl      # Original Lotus mining kernel (OpenCL)
│   ├── lotus_macos.metal# Metal kernel for macOS
│   └── poclbm120327.cl  # Alternative mining kernel (OpenCL)
├── lotus-miner-cli/     # Command-line interface
├── lotus-miner-lib/     # Core mining library
├── lotus-miner-gui/     # Graphical user interface (egui-based)
└── doc/                 # Documentation
```

## Prerequisites

### System Requirements

- **GPU**: OpenCL-compatible GPU (AMD, NVIDIA, or Intel)
- **OS**: Windows 10+, macOS 10.15+, or Linux (Ubuntu 20.04+)
- **Rust**: Rust 1.70+ (install via [rustup](https://rustup.rs/))
- **OpenCL**: OpenCL drivers for your GPU

### Installing OpenCL

**Linux (Ubuntu/Debian):**
```bash
# AMD GPUs
sudo apt-get install opencl-headers ocl-icd-opencl-dev
sudo apt-get install mesa-opencl-icd

# NVIDIA GPUs
sudo apt-get install nvidia-opencl-dev
```

**macOS:**

On macOS 11.0+ (Big Sur), the miner uses Metal compute shaders for Apple Silicon and AMD GPUs. OpenCL is deprecated by Apple.

```bash
# Install Xcode Command Line Tools (provides Metal framework)
xcode-select --install
```

**Requirements:**
- macOS 11.0+ (Big Sur or later)
- Apple Silicon (M1/M2/M3) or AMD GPU
- Xcode Command Line Tools

**Note for older macOS:** If you're on macOS 10.15 or earlier with an Intel GPU, you may need to use OpenCL. Contact the developers for legacy support.

**Windows:**
Install GPU-specific drivers:
- [AMD Adrenalin Drivers](https://www.amd.com/en/support)
- [NVIDIA GeForce Drivers](https://www.nvidia.com/Download/index.aspx)

## Installation

### Clone the Repository

```bash
git clone https://github.com/LotusiaStewardship/lotus-gpu-miner.git
cd lotus-gpu-miner
```

### Build from Source

```bash
# Build CLI version (default)
cargo build --release

# Build GUI version
cargo build --release -p lotus-miner-gui

# Build all components
cargo build --release --workspace
```

The binaries will be available in `target/release/`:
- `lotus-miner-cli` - Command-line miner
- `lotus-miner-gui` - Graphical interface miner

## Configuration

### Configuration File

The miner uses a TOML configuration file located at:
- **Linux/macOS**: `~/.lotus-miner/config.toml`
- **Windows**: `%USERPROFILE%\.lotus-miner\config.toml`

The configuration file is auto-generated on first run with default values:

```toml
# Mining reward address (required)
mine_to_address = "your-lotus-address-here"

# Lotus node RPC settings
rpc_url = "http://127.0.0.1:10604"
rpc_user = "lotus"
rpc_password = "lotus"
rpc_poll_interval = 3

# GPU settings
gpu_index = 0           # GPU device index
kernel_size = 23        # Mining intensity (higher = more GPU usage)

# Stratum mode
stratum_url = ""        # Leave empty for RPC mode, or "pool.example.com:3333" for stratum
stratum_worker_name = ""
stratum_password = "x"
```

### Command-Line Options

Both CLI and GUI support command-line configuration overrides:

```
OPTIONS:
    -c, --config <config>                  Configuration file path
    -a, --rpc-url <rpc_url>                Lotus RPC address
    -i, --rpc-poll-interval <interval>     RPC poll interval in seconds
    -u, --rpc-user <rpc_user>              RPC username
    -p, --rpc-password <rpc_password>      RPC password
    -o, --mine-to-address <address>        Coinbase output address
    -s, --kernel-size <size>               Kernel intensity size
    -g, --gpu-index <index>                GPU device index
        --stratum-url <url>                Stratum server host:port (enables stratum mode)
        --stratum-worker-name <name>       Worker name suffix
        --stratum-password <password>      Stratum password
```

## Usage

### CLI Miner

```bash
# Run with default configuration
./target/release/lotus-miner-cli

# Run with custom RPC endpoint
./target/release/lotus-miner-cli --rpc-url http://node.example.com:10604

# Run with stratum pool
./target/release/lotus-miner-cli --stratum-url pool.example.com:3333 --mine-to-address YOUR_ADDRESS

# Specify GPU and intensity
./target/release/lotus-miner-cli --gpu-index 0 --kernel-size 24
```

### GUI Miner

```bash
./target/release/lotus-miner-gui
```

The GUI provides:
- Real-time hashrate graphs
- Mining logs with severity levels
- Device selection and configuration
- Start/Stop mining controls
- Settings management

## Mining Modes

### RPC Mode (Direct Mining)

Connect directly to a Lotus node:

```toml
rpc_url = "http://127.0.0.1:10604"
stratum_url = ""  # Empty to use RPC mode
```

**Requirements:**
- Running Lotus node with RPC enabled
- Node must be fully synced

### Stratum Mode (Pool Mining)

Connect to a mining pool:

```toml
stratum_url = "pool.example.com:3333"
stratum_worker_name = "worker1"
stratum_password = "x"
```

The worker name is combined with your mining address as: `<address>.<worker_name>`

## Tuning Performance

### Kernel Size (Intensity)

The `kernel_size` parameter controls mining intensity:
- **Lower values (12-14)**: Less GPU usage, cooler operation, suitable for integrated GPUs
- **Medium values (15-18)**: Balanced performance for most dedicated GPUs
- **Higher values (19-23)**: Maximum performance, higher power consumption

**Recommendation:** Start with `kernel_size = 16` and adjust based on your GPU's thermal performance.

### GPU Selection

Use `gpu_index` to select which GPU to mine on:
```bash
# List available GPUs (shown at startup)
./target/release/lotus-miner-cli

# Select GPU 1
./target/release/lotus-miner-cli --gpu-index 1
```

### Multiple GPUs

Currently, the miner supports one GPU at a time. For multi-GPU setups, run multiple instances with different `gpu_index` values.

## Monitoring

### Hashrate Reporting

The miner reports hashrate every 10 seconds by default. Example output:
```
2026-05-02T05:56:52.468386-07:00 Hashrate 231.921 MH/s
```

### Logs

Mining logs are accessible via:
- **CLI**: Printed to stdout with severity levels (Info, Warn, Error)
- **GUI**: Displayed in the logs panel with color coding

## Troubleshooting

### Common Issues

**"No OpenCL platforms found"**
- Ensure OpenCL drivers are installed
- Check that your GPU is detected: `clinfo` (Linux) or GPU-Z (Windows)

**"GPU not found"**
- Verify GPU index with the platform listing at startup
- Try different `gpu_index` values

**Low hashrate**
- Increase `kernel_size` gradually
- Ensure GPU is not thermal throttling
- Close other GPU-intensive applications

**Connection errors**
- Verify Lotus node is running and RPC is enabled
- Check firewall settings for port 10604
- For stratum: verify pool URL and credentials

### Getting Help

- Check the [Lotus Documentation](https://lotusia.org/docs)
- Join the Lotus community Discord/Telegram
- Review existing GitHub issues

## Development

### Project Dependencies

Key Rust crates used:
- `ocl` - OpenCL bindings
- `tokio` - Async runtime
- `eframe`/`egui` - GUI framework
- `serde` - Serialization
- `reqwest` - HTTP client
- `clap` - CLI argument parsing

### Building Kernels

Mining kernels are written in OpenCL C and located in `kernels/`:
- `lotus_og.cl` - Original Lotus kernel
- `poclbm120327.cl` - Alternative kernel based on BTCMiner

To add a new kernel:
1. Create `.cl` file in `kernels/`
2. Reference it in config with `kernel_name` parameter

### Testing

```bash
# Run tests
cargo test

# Run with verbose logging
RUST_LOG=debug cargo run --release
```

## Security Considerations

- **Private Keys**: Never share your mining address private key
- **RPC Credentials**: Use strong passwords for RPC authentication
- **Config File**: Protect `config.toml` with appropriate file permissions
- **Pool Selection**: Only use reputable mining pools

## License

This project is licensed under the [MIT License](LICENSE).

## Credits

- **Author**: Tobias Ruck <ruck.tobias@gmail.com>
- **Copyright**: Logos Foundation (2021)
- **Contributors**: See GitHub contributors page

## Disclaimer

Mining cryptocurrencies involves financial risk. GPU mining consumes significant electricity and may void hardware warranties. Only mine with hardware you own and understand the associated costs in your jurisdiction.

---

For more information about the Lotus Network, visit [lotusnetwork.io](https://lotusia.org/).
