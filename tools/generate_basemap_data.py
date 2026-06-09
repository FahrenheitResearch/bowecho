import math
import struct
import unicodedata
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOURCE_DIR = ROOT / "work" / "basemap_sources"
OUT = ROOT / "crates" / "app_ui" / "src" / "basemap_data.rs"

REGIONS = {
    "US": {"codes": {"USA"}, "bbox": (-170.0, 18.0, -50.0, 72.0)},
    "CANADA": {"codes": {"CAN"}, "bbox": (-142.0, 41.0, -52.0, 84.0)},
    "MEXICO": {"codes": {"MEX"}, "bbox": (-119.0, 14.0, -86.0, 34.0)},
    "JAPAN": {"codes": {"JPN"}, "bbox": (122.0, 24.0, 153.0, 46.5)},
}


def read_zip_member(zip_name, suffix):
    with zipfile.ZipFile(SOURCE_DIR / zip_name) as archive:
        name = next(name for name in archive.namelist() if name.endswith(suffix))
        return archive.read(name)


def read_zip_text(zip_name, suffix, default=None):
    with zipfile.ZipFile(SOURCE_DIR / zip_name) as archive:
        matches = [name for name in archive.namelist() if name.endswith(suffix)]
        if not matches:
            return default
        return archive.read(matches[0]).decode("ascii", "ignore").strip()


def dbf_encoding(zip_name):
    cpg = (read_zip_text(zip_name, ".cpg", "latin1") or "latin1").lower()
    if cpg in {"65001", "utf-8", "utf8"}:
        return "utf-8"
    return cpg


def read_dbf(zip_name):
    data = read_zip_member(zip_name, ".dbf")
    encoding = dbf_encoding(zip_name)
    record_count = struct.unpack_from("<I", data, 4)[0]
    header_len = struct.unpack_from("<H", data, 8)[0]
    record_len = struct.unpack_from("<H", data, 10)[0]
    fields = []
    offset = 1
    pos = 32
    while data[pos] != 0x0D:
        raw_name = data[pos : pos + 11].split(b"\x00", 1)[0]
        name = raw_name.decode("ascii", "ignore")
        field_type = chr(data[pos + 11])
        length = data[pos + 16]
        fields.append((name, field_type, offset, length))
        offset += length
        pos += 32

    records = []
    for record_index in range(record_count):
        start = header_len + record_index * record_len
        if start + record_len > len(data) or data[start : start + 1] == b"*":
            records.append({})
            continue
        row = {}
        for name, field_type, field_offset, length in fields:
            raw = data[start + field_offset : start + field_offset + length]
            text = raw.decode(encoding, "ignore").replace("\x00", "").strip()
            if field_type in ("N", "F"):
                if text == "":
                    value = 0.0
                else:
                    try:
                        value = float(text)
                    except ValueError:
                        value = 0.0
            else:
                value = text
            row[name] = value
        records.append(row)
    return records


def read_shapes(zip_name):
    data = read_zip_member(zip_name, ".shp")
    shapes = []
    pos = 100
    while pos + 8 <= len(data):
        content_words = struct.unpack_from(">i", data, pos + 4)[0]
        content_len = content_words * 2
        content_start = pos + 8
        content_end = content_start + content_len
        if content_end > len(data):
            break
        shape_type = struct.unpack_from("<i", data, content_start)[0]
        if shape_type == 0:
            shapes.append(None)
        elif shape_type == 1:
            x, y = struct.unpack_from("<2d", data, content_start + 4)
            shapes.append({"type": "point", "point": (x, y)})
        elif shape_type in (3, 5):
            xmin, ymin, xmax, ymax = struct.unpack_from("<4d", data, content_start + 4)
            num_parts, num_points = struct.unpack_from("<2i", data, content_start + 36)
            parts_offset = content_start + 44
            points_offset = parts_offset + num_parts * 4
            parts = [
                struct.unpack_from("<i", data, parts_offset + index * 4)[0]
                for index in range(num_parts)
            ]
            points = [
                struct.unpack_from("<2d", data, points_offset + index * 16)
                for index in range(num_points)
            ]
            rings = []
            for part_index, start in enumerate(parts):
                end = parts[part_index + 1] if part_index + 1 < len(parts) else num_points
                rings.append(points[start:end])
            shapes.append(
                {
                    "type": "line",
                    "bbox": (xmin, ymin, xmax, ymax),
                    "rings": rings,
                }
            )
        else:
            shapes.append(None)
        pos = content_end
    return shapes


def perpendicular_distance(point, start, end):
    x, y = point
    x1, y1 = start
    x2, y2 = end
    dx = x2 - x1
    dy = y2 - y1
    if dx == 0.0 and dy == 0.0:
        return math.hypot(x - x1, y - y1)
    return abs(dy * x - dx * y + x2 * y1 - y2 * x1) / math.hypot(dx, dy)


def simplify(points, tolerance):
    if len(points) <= 2:
        return points
    closed = points[0] == points[-1]
    work = points[:-1] if closed else points
    if len(work) <= 2:
        return points

    keep = [False] * len(work)
    keep[0] = True
    keep[-1] = True
    stack = [(0, len(work) - 1)]
    while stack:
        start, end = stack.pop()
        max_distance = -1.0
        max_index = None
        for index in range(start + 1, end):
            distance = perpendicular_distance(work[index], work[start], work[end])
            if distance > max_distance:
                max_distance = distance
                max_index = index
        if max_index is not None and max_distance > tolerance:
            keep[max_index] = True
            stack.append((start, max_index))
            stack.append((max_index, end))

    simplified = [point for point, should_keep in zip(work, keep) if should_keep]
    if closed and simplified[0] != simplified[-1]:
        simplified.append(simplified[0])
    return simplified


def bbox_for(points):
    xs = [point[0] for point in points]
    ys = [point[1] for point in points]
    return (min(xs), min(ys), max(xs), max(ys))


def bbox_union(left, right):
    if left is None:
        return right
    return (
        min(left[0], right[0]),
        min(left[1], right[1]),
        max(left[2], right[2]),
        max(left[3], right[3]),
    )


def bbox_intersects(left, right):
    return left[2] >= right[0] and left[0] <= right[2] and left[3] >= right[1] and left[1] <= right[3]


def ring_area(points):
    if len(points) < 3:
        return 0.0
    area = 0.0
    for left, right in zip(points, points[1:]):
        area += left[0] * right[1] - right[0] * left[1]
    return abs(area) * 0.5


def clean_text(value):
    value = str(value).replace("\\", "").replace('"', "").replace("\x00", "").strip()
    normalized = unicodedata.normalize("NFKD", value)
    return normalized.encode("ascii", "ignore").decode("ascii").strip()


def country_code(record):
    return clean_text(record.get("ADM0_A3") or record.get("adm0_a3") or record.get("SOV_A3"))


def make_lines(zip_name, tolerance, min_area, record_filter=None, name_field=None, state_field=None):
    shapes = read_shapes(zip_name)
    records = read_dbf(zip_name)
    lines = []
    labels = []
    for shape, record in zip(shapes, records):
        if not shape or shape["type"] != "line":
            continue
        if record_filter and not record_filter(record, shape):
            continue
        largest_ring = None
        largest_area = 0.0
        for ring in shape["rings"]:
            if len(ring) < 2:
                continue
            area = ring_area(ring)
            if area < min_area:
                continue
            if area > largest_area:
                largest_area = area
                largest_ring = ring
            simplified = simplify(ring, tolerance)
            if len(simplified) < 2:
                continue
            lines.append((bbox_for(simplified), simplified))
        if name_field and largest_ring:
            bbox = bbox_for(largest_ring)
            name = clean_text(record.get(name_field, ""))
            state = clean_text(record.get(state_field, "")) if state_field else ""
            if name:
                labels.append(
                    {
                        "name": name,
                        "state": state,
                        "lon": (bbox[0] + bbox[2]) * 0.5,
                        "lat": (bbox[1] + bbox[3]) * 0.5,
                        "bbox": bbox,
                    }
                )
    return lines, labels


def make_region_admin_lines(region_name):
    codes = REGIONS[region_name]["codes"]
    region_bbox = REGIONS[region_name]["bbox"]
    return make_lines(
        "ne_10m_admin_1_states_provinces.zip",
        tolerance=0.01,
        min_area=0.00002,
        name_field="name_en",
        record_filter=lambda record, shape: country_code(record) in codes
        and bbox_intersects(shape["bbox"], region_bbox),
    )


def read_places():
    shapes = read_shapes("ne_10m_populated_places.zip")
    records = read_dbf("ne_10m_populated_places.zip")
    regional = {region: [] for region in REGIONS}
    world = []
    for shape, record in zip(shapes, records):
        if not shape or shape["type"] != "point":
            continue
        lon, lat = shape["point"]
        code = country_code(record)
        population = int(record.get("POP_MAX", 0.0))
        scalerank = int(record.get("SCALERANK", 10.0))
        if population >= 5_000_000:
            rank = 0
        elif population >= 1_000_000:
            rank = 1
        elif population >= 250_000:
            rank = 2
        elif population >= 100_000:
            rank = 3
        elif population >= 50_000:
            rank = 4
        elif population >= 20_000:
            rank = 5
        elif population >= 10_000:
            rank = 6
        else:
            rank = 7
        rank = min(rank, scalerank)
        name = clean_text(record.get("NAMEASCII") or record.get("NAME_EN") or record.get("NAME") or "")
        if not name:
            continue
        place = {
            "name": name,
            "lon": lon,
            "lat": lat,
            "population": population,
            "rank": rank,
        }
        if rank <= 1:
            world.append(place)
        for region, spec in REGIONS.items():
            if code in spec["codes"] and bbox_intersects((lon, lat, lon, lat), spec["bbox"]):
                if region == "US" and population >= 1_000:
                    regional[region].append(place)
                elif region != "US" and population >= 5_000:
                    regional[region].append(place)
    for places in [world, *regional.values()]:
        places.sort(key=lambda place: (place["rank"], -place["population"], place["name"]))
    return world, regional


def rust_float(value):
    return f"{value:.5f}"


def write_lines(handle, const_name, lines):
    handle.write("#[rustfmt::skip]\n")
    handle.write(f"pub const {const_name}: &[BasemapLine] = &[\n")
    for bbox, points in lines:
        handle.write(
            "    BasemapLine { bbox: ["
            + ", ".join(rust_float(value) for value in bbox)
            + "], points: &[\n"
        )
        for lon, lat in points:
            handle.write(f"        ({rust_float(lon)}, {rust_float(lat)}),\n")
        handle.write("    ] },\n")
    handle.write("];\n\n")


def write_labels(handle, const_name, labels, default_rank=None):
    handle.write("#[rustfmt::skip]\n")
    handle.write(f"pub const {const_name}: &[BasemapLabel] = &[\n")
    for label in labels:
        rank = default_rank if default_rank is not None else int(label["rank"])
        handle.write(
            f"    BasemapLabel {{ name: \"{label['name']}\", "
            f"lon: {rust_float(label['lon'])}, lat: {rust_float(label['lat'])}, "
            f"rank: {rank} }},\n"
        )
    handle.write("];\n\n")


def write_bbox(handle, const_name, bbox):
    handle.write(
        f"pub const {const_name}: [f32; 4] = ["
        + ", ".join(rust_float(value) for value in bbox)
        + "];\n"
    )


def main():
    world_country_lines, _ = make_lines(
        "ne_50m_admin_0_countries.zip", tolerance=0.03, min_area=0.002
    )
    us_state_lines, _ = make_lines(
        "cb_2024_us_state_500k.zip", tolerance=0.004, min_area=0.00002
    )
    us_county_lines, us_county_labels = make_lines(
        "cb_2024_us_county_500k.zip",
        tolerance=0.006,
        min_area=0.00001,
        name_field="NAME",
        state_field="STATEFP",
    )
    regional_admin = {
        "CANADA": make_region_admin_lines("CANADA"),
        "MEXICO": make_region_admin_lines("MEXICO"),
        "JAPAN": make_region_admin_lines("JAPAN"),
    }
    world_places, regional_places = read_places()

    OUT.parent.mkdir(parents=True, exist_ok=True)
    with OUT.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write("// Generated by tools/generate_basemap_data.py.\n")
        handle.write(
            "// Source data: US Census 2024 cartographic boundaries and Natural Earth boundaries/places.\n\n"
        )
        handle.write("#![allow(clippy::approx_constant, clippy::excessive_precision)]\n\n")
        handle.write("#[derive(Clone, Copy, Debug)]\n")
        handle.write("pub struct BasemapLine {\n")
        handle.write("    pub bbox: [f32; 4],\n")
        handle.write("    pub points: &'static [(f32, f32)],\n")
        handle.write("}\n\n")
        handle.write("#[derive(Clone, Copy, Debug)]\n")
        handle.write("pub struct BasemapLabel {\n")
        handle.write("    pub name: &'static str,\n")
        handle.write("    pub lon: f32,\n")
        handle.write("    pub lat: f32,\n")
        handle.write("    pub rank: u8,\n")
        handle.write("}\n\n")
        for region, spec in REGIONS.items():
            write_bbox(handle, f"BASEMAP_{region}_BOUNDS", spec["bbox"])
        handle.write("\n")
        write_lines(handle, "BASEMAP_WORLD_COUNTRY_LINES", world_country_lines)
        write_lines(handle, "BASEMAP_US_STATE_LINES", us_state_lines)
        write_lines(handle, "BASEMAP_US_COUNTY_LINES", us_county_lines)
        for region, (lines, labels) in regional_admin.items():
            write_lines(handle, f"BASEMAP_{region}_ADMIN_LINES", lines)
            write_labels(handle, f"BASEMAP_{region}_ADMIN_LABELS", labels, default_rank=4)
        write_labels(handle, "BASEMAP_US_COUNTY_LABELS", us_county_labels, default_rank=4)
        write_labels(handle, "BASEMAP_WORLD_PLACE_LABELS", world_places)
        for region, places in regional_places.items():
            write_labels(handle, f"BASEMAP_{region}_PLACE_LABELS", places)

    content = OUT.read_text(encoding="utf-8")
    OUT.write_text(content.rstrip() + "\n", encoding="utf-8", newline="\n")

    print(
        " ".join(
            [
                f"world_country_lines={len(world_country_lines)}",
                f"us_state_lines={len(us_state_lines)}",
                f"us_county_lines={len(us_county_lines)}",
                f"us_county_labels={len(us_county_labels)}",
                f"world_places={len(world_places)}",
                *(f"{region.lower()}_admin_lines={len(lines)} {region.lower()}_places={len(regional_places[region])}" for region, (lines, _) in regional_admin.items()),
            ]
        )
    )
    print(f"wrote={OUT} bytes={OUT.stat().st_size}")


if __name__ == "__main__":
    main()
