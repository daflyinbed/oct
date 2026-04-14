#!/usr/bin/env bash
set -e

echo "=== Building Oct Agent ==="

# Build frontend
echo "Building frontend..."
cd frontend
npm install --legacy-peer-deps
npm run build
cd ..

# Build backend
echo "Building backend..."
cargo build -p oct-agent --release

echo ""
echo "Build complete!"
echo "Binary: target/release/oct-agent"
echo "Frontend: frontend/dist/"
