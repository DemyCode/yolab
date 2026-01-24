# YoLab Client UI

React + TypeScript web interface for managing YoLab client configuration and services.

## Features

- Edit config.toml through web interface
- Browse and download available services from platform
- View downloaded services
- Trigger system rebuild

## Development

Backend:
```bash
uv sync
cp .env.example .env
uv run python backend.py
```

Frontend:
```bash
cd frontend
npm install
npm run dev
```

Visit http://localhost:8080 (backend) or http://localhost:5173 (frontend dev server)

## Production Build

```bash
cd frontend
npm install
npm run build
```

The backend will serve the built React app from `frontend/dist/`

## Configuration

Set environment variables in `.env`:

- `PLATFORM_API_URL` - URL of the YoLab platform backend
- `CONFIG_PATH` - Path to config.toml
- `SERVICES_DIR` - Directory for downloaded services

## API Endpoints

- `GET /` - Web interface
- `GET /config` - Get config.toml
- `POST /config` - Update config.toml
- `GET /services/available` - List available services
- `GET /services/downloaded` - List downloaded services
- `POST /services/download/{name}` - Download service
- `POST /services/delete/{name}` - Delete service
- `POST /rebuild` - Trigger nixos-rebuild
