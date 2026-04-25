import base64
import json
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
type_cache = {}


def resolve_message_type(type_name):
    if type_name not in type_cache:
        type_cache[type_name] = get_message(type_name)
    return type_cache[type_name]


def lookup_field(value, path):
    current = value
    for segment in path.split("."):
        if not isinstance(current, dict) or segment not in current:
            raise KeyError(segment)
        current = current[segment]
    return current


def flatten_fields(obj, prefix=""):
    """Recursively flatten a dict/list into a list of (key, value) pairs."""
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


def render_message(type_name, payload_b64):
    global csv_header_printed

    msg_type = resolve_message_type(type_name)
    payload = base64.b64decode(payload_b64)
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
        data = message_to_ordereddict(
            message,
            truncate_length=truncate_length,
            no_arr=no_arr,
            no_str=no_str,
        )
        try:
            selected = lookup_field(data, field_path)
        except KeyError as exc:
            raise RuntimeError(f"field '{field_path}' not found in {type_name}") from exc
        if isinstance(selected, (dict, list)):
            return json.dumps(selected, ensure_ascii=False)
        if isinstance(selected, str):
            return selected
        return str(selected)

    return message_to_yaml(
        message,
        truncate_length=truncate_length,
        no_arr=no_arr,
        no_str=no_str,
    ).rstrip()


def send_ok(rendered):
    sys.stdout.write(json.dumps({"status": "ok", "rendered": rendered}) + "\n")
    sys.stdout.flush()


def send_err(error):
    sys.stdout.write(json.dumps({"status": "err", "error": str(error)}) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    if not line.strip():
        continue
    request = json.loads(line)
    kind = request["kind"]
    try:
        if kind == "init":
            field_path = request.get("field")
            truncate_length = request["truncate_length"]
            no_arr = request.get("no_arr", False)
            no_str = request.get("no_str", False)
            csv_mode = request.get("csv", False)
            csv_header_printed = False
            send_ok("")
        elif kind == "decode":
            send_ok(render_message(request["type_name"], request["payload_base64"]))
        else:
            send_err(f"unknown request kind {kind}")
    except Exception as exc:
        send_err(exc)
