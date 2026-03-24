#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
python -m grpc_tools.protoc \
  -I. \
  -I"$(python -c 'import grpc_tools; import os; print(os.path.dirname(grpc_tools.__file__))')" \
  --python_out=node_agent/csi \
  --grpc_python_out=node_agent/csi \
  csi.proto
sed -i 's/^import csi_pb2/from node_agent.csi import csi_pb2/' node_agent/csi/csi_pb2_grpc.py
