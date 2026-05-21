import json
import socket
import struct
import sys

from rclpy.serialization import deserialize_message
from rosidl_runtime_py.convert import message_to_ordereddict, message_to_yaml
from rosidl_runtime_py.utilities import get_message


field_path = None
truncate_length = 128
no_arr = False
no_str = False
csv_mode = False
csv_header_printed = False
default_type = None
type_cache = {}


def resolve_message_type(type_name):
    if type_name not in type_cache:
        type_cache[type_name] = get_message(type_name)
    return type_cache[type_name]


def read_exact(sock, n):
    chunks = []
    remaining = n
    while remaining:
        chunk = sock.recv(remaining)
        if not chunk:
            raise EOFError("topic echo stream closed")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_frame(sock):
    header = read_exact(sock, 19)
    if header[0:4] != b"RPE1":
        raise RuntimeError("invalid topic echo stream frame")
    if header[4] != 1:
        raise RuntimeError(f"unsupported topic echo stream version {header[4]}")
    type_len = struct.unpack("<H", header[5:7])[0]
    lost_before = struct.unpack("<Q", header[7:15])[0]
    payload_len = struct.unpack("<I", header[15:19])[0]
    type_name = read_exact(sock, type_len).decode("utf-8") if type_len else ""
    payload = read_exact(sock, payload_len)
    return type_name, lost_before, payload


def lookup_field(value, path):
    current = value
    for segment in path.split("."):
        current = getattr(current, segment)
    return current


def flatten_fields(obj, prefix=""):
    result = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            child = f"{prefix}.{k}" if prefix else k
            result.extend(flatten_fields(v, child))
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            child = f"{prefix}.{i}" if prefix else str(i)
            result.extend(flatten_fields(v, child))
    else:
        result.append((prefix, obj))
    return result


def csv_escape(value):
    s = str(value)
    if "," in s or '"' in s or "\n" in s:
        s = '"' + s.replace('"', '""') + '"'
    return s


def render_selected_field(message, type_name):
    try:
        selected = lookup_field(message, field_path)
    except AttributeError as exc:
        raise RuntimeError(f"field '{field_path}' not found in {type_name}") from exc
    if hasattr(selected, "get_fields_and_field_types"):
        return message_to_yaml(
            selected,
            truncate_length=truncate_length,
            no_arr=no_arr,
            no_str=no_str,
        ).rstrip()
    if isinstance(selected, (list, tuple)):
        return json.dumps(list(selected), ensure_ascii=False)
    if isinstance(selected, (bytes, bytearray)):
        return str(list(selected[:truncate_length]))
    return str(selected)


def render_message(type_name, payload):
    global csv_header_printed

    effective_type = type_name or default_type
    if not effective_type:
        raise RuntimeError("missing message type for decoded echo")
    msg_type = resolve_message_type(effective_type)
    message = deserialize_message(payload, msg_type)

    if csv_mode:
        data = message_to_ordereddict(
            message,
            truncate_length=truncate_length,
            no_arr=no_arr,
            no_str=no_str,
        )
        fields = flatten_fields(data)
        values_line = ",".join(csv_escape(v) for _, v in fields)
        if not csv_header_printed:
            csv_header_printed = True
            header_line = ",".join(csv_escape(k) for k, _ in fields)
            return f"{header_line}\n{values_line}"
        return values_line

    if field_path:
        return render_selected_field(message, effective_type)

    return message_to_yaml(
        message,
        truncate_length=truncate_length,
        no_arr=no_arr,
        no_str=no_str,
    ).rstrip()


def send_ok(rendered, lost_before=0):
    sys.stdout.write(
        json.dumps({"status": "ok", "rendered": rendered, "lost_before": lost_before}) + "\n"
    )
    sys.stdout.flush()


def send_err(error):
    sys.stdout.write(json.dumps({"status": "err", "error": str(error)}) + "\n")
    sys.stdout.flush()


init_line = sys.stdin.readline()
if not init_line.strip():
    send_err("missing init request")
    sys.exit(1)

try:
    request = json.loads(init_line)
    if request["kind"] != "init":
        raise RuntimeError(f"expected init request, got {request['kind']}")
    field_path = request.get("field")
    truncate_length = request["truncate_length"]
    no_arr = request.get("no_arr", False)
    no_str = request.get("no_str", False)
    csv_mode = request.get("csv", False)
    csv_header_printed = False
    default_type = request.get("default_type")
    stream_path = request["stream_path"]
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(stream_path)
    send_ok("")
except Exception as exc:
    send_err(exc)
    sys.exit(1)

while True:
    try:
        type_name, lost_before, payload = read_frame(sock)
        send_ok(render_message(type_name, payload), lost_before)
    except EOFError:
        break
    except Exception as exc:
        send_err(exc)
