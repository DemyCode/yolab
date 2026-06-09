from typing import Any

from pydantic import BaseModel


class AppOutput(BaseModel):
    key: str
    label: str
    value: str
    type: str


class OutputSpec(BaseModel):
    key: str
    label: str
    type: str


class AppInfo(BaseModel):
    app_id: str
    instance_name: str
    status: str
    outputs: list[AppOutput]
    outputs_spec: list[OutputSpec]
    config: dict[str, Any]


class CatalogApp(BaseModel):
    id: str
    name: str
    description: str
    icon: str
    category: str
    schema: dict[str, Any]
    uischema: dict[str, Any]


class PodInfo(BaseModel):
    name: str
    phase: str
    ready: bool


class DescribeResponse(BaseModel):
    output: str


class ScanOutputsResponse(BaseModel):
    outputs: list[AppOutput]


class DomainResponse(BaseModel):
    domain: str


class RebuildLog(BaseModel):
    running: bool
    log: list[str]
