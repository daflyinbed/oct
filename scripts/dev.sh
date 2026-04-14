#!/usr/bin/env bash
set -e

echo "=== Starting Oct Agent Development ==="

# Start backend
echo "Starting backend on :3000..."
cargo run -p oct-agent &
BACKEND_PID=$!

# Start frontend dev server
echo "Starting frontend on :5173..."
cd frontend
npm run dev &
FRONTEND_PID=$!

trap "kill $BACKEND_PID $FRONTEND_PID 2>/dev/null; exit" INT TERM

echo ""
echo "Backend:  http://localhost:3000"
echo "Frontend: http://localhost:5173"
echo "API docs: http://localhost:3000/scalar"
echo ""
echo "Press Ctrl+C to stop."

wait
