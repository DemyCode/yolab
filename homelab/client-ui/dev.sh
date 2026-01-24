#!/bin/bash

echo "Starting YoLab Client UI Development Environment"
echo ""

if [ ! -f .env ]; then
    echo "Creating .env from .env.example..."
    cp .env.example .env
fi

cd frontend

if [ ! -d node_modules ]; then
    echo "Installing frontend dependencies..."
    npm install
fi

echo ""
echo "Building frontend..."
npm run build

cd ..

echo ""
echo "Starting backend server on http://localhost:8080"
echo ""

uv run python backend.py
