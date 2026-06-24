#
# Copyright 2026 Alibaba Group Holding Ltd.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
"""PTY session models."""

from pydantic import BaseModel, ConfigDict, Field

PTY_BINARY_STDIN = 0x00
PTY_BINARY_STDOUT = 0x01
PTY_BINARY_STDERR = 0x02
PTY_BINARY_REPLAY = 0x03


class CreatePTYSessionRequest(BaseModel):
    """Request body for creating a PTY session."""

    cwd: str | None = Field(default=None)
    command: str | None = Field(default=None)


class CreatePTYSessionResponse(BaseModel):
    """Response from creating a PTY session."""

    session_id: str


class PTYSessionStatus(BaseModel):
    """Status for an existing PTY session."""

    session_id: str
    running: bool
    output_offset: int


class PTYClientFrame(BaseModel):
    """JSON frame sent from client to PTY WebSocket."""

    type: str
    data: str | None = None
    cols: int | None = None
    rows: int | None = None
    signal: str | None = None


class PTYServerFrame(BaseModel):
    """JSON frame sent from PTY WebSocket to client."""

    type: str
    session_id: str | None = None
    mode: str | None = None
    data: str | None = None
    offset: int | None = None
    exit_code: int | None = Field(default=None, alias="exit_code")
    error: str | None = None
    code: str | None = None
    timestamp: int | None = None

    model_config = ConfigDict(populate_by_name=True)


def encode_pty_stdin(data: bytes | str) -> bytes:
    """Encode stdin bytes for the PTY binary WebSocket protocol."""
    payload = data.encode() if isinstance(data, str) else data
    return bytes([PTY_BINARY_STDIN]) + payload
