import base64
import json
import sys

from rclpy.serialization import deserialize_message
from rosidl_runtime_py.utilities import get_message


type_cache = {}


def resolve_message_type(type_name):
    if type_name not in type_cache:
        type_cache[type_name] = get_message(type_name)
    return type_cache[type_name]


def extract_delay_seconds(type_name, payload_b64, received_at_nanos):
    msg_type = resolve_message_type(type_name)
    payload = base64.b64decode(payload_b64)
    message = deserialize_message(payload, msg_type)

    if not hasattr(message, "header"):
        return {"status": "missing_header"}
    header = getattr(message, "header")
    if not hasattr(header, "stamp"):
        return {"status": "missing_header"}
    stamp = getattr(header, "stamp")
    if not hasattr(stamp, "sec") or not hasattr(stamp, "nanosec"):
        return {"status": "missing_header"}

    stamp_nanos = int(stamp.sec) * 1_000_000_000 + int(stamp.nanosec)
    delay_seconds = (int(received_at_nanos) - stamp_nanos) / 1_000_000_000.0
    return {"status": "delay", "delay_seconds": delay_seconds}


for line in sys.stdin:
    if not line.strip():
        continue
    request = json.loads(line)
    try:
        if request["kind"] == "extract":
            response = extract_delay_seconds(
                request["type_name"],
                request["payload_base64"],
                request["received_at_nanos"],
            )
        else:
            response = {"status": "err", "error": f"unknown request kind {request['kind']}"}
    except Exception as exc:
        response = {"status": "err", "error": str(exc)}
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
