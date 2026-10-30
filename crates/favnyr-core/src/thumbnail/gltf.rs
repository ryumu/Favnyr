//! Static glTF 2.0 and GLB geometry previews, with local bounded resources.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;

use super::{Thumbnail, mesh};

const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;
const MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_384;
const MAX_NODES: usize = 4096;
type Matrix = [f64; 16];
struct Container {
    json: Vec<u8>,
    bin: Option<(u64, usize)>,
}
const IDENTITY: Matrix = [
    1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.,
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    asset: Asset,
    #[serde(default)]
    extensions_required: Vec<String>,
    scene: Option<usize>,
    #[serde(default, deserialize_with = "limited_vec")]
    scenes: Vec<Scene>,
    #[serde(default, deserialize_with = "limited_vec")]
    nodes: Vec<Node>,
    #[serde(default, deserialize_with = "limited_vec")]
    meshes: Vec<Mesh>,
    #[serde(default, deserialize_with = "limited_vec")]
    accessors: Vec<Accessor>,
    #[serde(default, deserialize_with = "limited_vec")]
    buffer_views: Vec<BufferView>,
    #[serde(default, deserialize_with = "limited_vec")]
    buffers: Vec<Buffer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Asset {
    version: String,
    min_version: Option<String>,
}
#[derive(Deserialize)]
struct Scene {
    #[serde(default, deserialize_with = "limited_vec")]
    nodes: Vec<usize>,
}
#[derive(Deserialize)]
struct Node {
    mesh: Option<usize>,
    skin: Option<usize>,
    #[serde(default, deserialize_with = "limited_vec")]
    children: Vec<usize>,
    matrix: Option<Matrix>,
    translation: Option<[f64; 3]>,
    rotation: Option<[f64; 4]>,
    scale: Option<[f64; 3]>,
}
#[derive(Deserialize)]
struct Mesh {
    #[serde(deserialize_with = "limited_vec")]
    primitives: Vec<Primitive>,
}
#[derive(Deserialize)]
struct Primitive {
    attributes: Attributes,
    indices: Option<usize>,
    mode: Option<u32>,
    #[serde(default, deserialize_with = "limited_vec")]
    targets: Vec<serde::de::IgnoredAny>,
}
#[derive(Deserialize)]
struct Attributes {
    #[serde(rename = "POSITION")]
    position: Option<usize>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Accessor {
    buffer_view: Option<usize>,
    #[serde(default)]
    byte_offset: usize,
    component_type: u32,
    count: usize,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    normalized: bool,
    sparse: Option<Sparse>,
}
#[derive(Deserialize)]
struct Sparse {
    count: usize,
    indices: SparseIndices,
    values: SparseValues,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SparseIndices {
    buffer_view: usize,
    #[serde(default)]
    byte_offset: usize,
    component_type: u32,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SparseValues {
    buffer_view: usize,
    #[serde(default)]
    byte_offset: usize,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BufferView {
    buffer: usize,
    #[serde(default)]
    byte_offset: usize,
    byte_length: usize,
    byte_stride: Option<usize>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Buffer {
    byte_length: usize,
    uri: Option<String>,
}

/// Bound structural arrays while deserializing, before a small JSON document
/// can expand into hundreds of thousands of heap-allocated scene records.
fn limited_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Limited<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Limited<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a bounded array")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element()? {
                if values.len() >= MAX_ENTRIES {
                    return Err(serde::de::Error::custom(
                        "model array exceeds preview limit",
                    ));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Limited(std::marker::PhantomData))
}

pub(super) fn from_gltf(path: &Path, max_px: u32) -> Option<Thumbnail> {
    let deadline = Instant::now() + mesh::TIME_LIMIT;
    let mut file = File::open(path).ok()?;
    let before = file.metadata().ok()?;
    if !before.is_file() || max_px == 0 {
        return None;
    }
    let Container { json, bin } = container(&mut file, before.len())?;
    let doc = document(&json)?;
    let instances = instances(&doc, deadline)?;
    let root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .ok()?;
    let mut buffers = Vec::new();
    let mut total = 0usize;
    for (index, buffer) in doc.buffers.iter().enumerate() {
        if Instant::now() >= deadline {
            return None;
        }
        total = total.checked_add(buffer.byte_length)?;
        if buffer.byte_length == 0 || total > MAX_BUFFER_BYTES {
            return None;
        }
        let data = if let Some(uri) = &buffer.uri {
            if uri.starts_with("data:") {
                data_uri(uri, buffer.byte_length)?
            } else {
                let resource = local_resource(&root, uri)?;
                if !resource.metadata().ok()?.is_file() {
                    return None;
                }
                if !crate::fs::content_is_local(&resource) {
                    return None;
                }
                let mut source = File::open(resource).ok()?;
                let metadata = source.metadata().ok()?;
                if !metadata.is_file() || metadata.len() < buffer.byte_length as u64 {
                    return None;
                }
                let data = read_bytes(&mut source, buffer.byte_length)?;
                let after = source.metadata().ok()?;
                if metadata.len() != after.len()
                    || metadata.modified().ok() != after.modified().ok()
                {
                    return None;
                }
                data
            }
        } else {
            if index != 0 {
                return None;
            }
            let (offset, length) = bin?;
            if length < buffer.byte_length || length - buffer.byte_length > 3 {
                return None;
            }
            file.seek(SeekFrom::Start(offset)).ok()?;
            read_bytes(&mut file, buffer.byte_length)?
        };
        buffers.push(data);
    }
    let result = mesh::render(max_px, deadline, |consume| {
        visit(&doc, &buffers, &instances, deadline, consume)
    })?;
    let after = file.metadata().ok()?;
    (before.len() == after.len() && before.modified().ok() == after.modified().ok())
        .then_some(result)
}

fn document(json: &[u8]) -> Option<Document> {
    if json.len() > MAX_JSON_BYTES {
        return None;
    }
    let doc: Document = serde_json::from_slice(json).ok()?;
    if doc.asset.version != "2.0"
        || doc
            .asset
            .min_version
            .as_deref()
            .is_some_and(|version| version != "2.0")
        || !doc.extensions_required.is_empty()
        || doc.nodes.len() > MAX_NODES
    {
        return None;
    }
    Some(doc)
}

fn read_bytes(reader: &mut impl Read, length: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// Keep GLB binary data on disk until buffers are requested, avoiding a second
/// full copy of a potentially large payload alongside the parsed document.
fn container<R: Read + Seek>(reader: &mut R, length: u64) -> Option<Container> {
    if length > (MAX_JSON_BYTES + MAX_BUFFER_BYTES + 1024) as u64 {
        return None;
    }
    let mut header = [0; 12];
    reader.read_exact(&mut header[..4]).ok()?;
    if &header[..4] != b"glTF" {
        if length > MAX_JSON_BYTES as u64 {
            return None;
        }
        reader.rewind().ok()?;
        return Some(Container {
            json: read_bytes(reader, length as usize)?,
            bin: None,
        });
    }
    reader.read_exact(&mut header[4..]).ok()?;
    if u32::from_le_bytes(header[4..8].try_into().ok()?) != 2
        || u64::from(u32::from_le_bytes(header[8..12].try_into().ok()?)) != length
    {
        return None;
    }
    let mut offset = 12u64;
    let mut json = None;
    let mut bin = None;
    let mut chunks = 0;
    while offset < length {
        chunks += 1;
        if chunks > 64 || length - offset < 8 {
            return None;
        }
        let mut chunk = [0; 8];
        reader.read_exact(&mut chunk).ok()?;
        let size = u32::from_le_bytes(chunk[..4].try_into().ok()?) as usize;
        let kind = &chunk[4..];
        offset += 8;
        if !size.is_multiple_of(4) || size as u64 > length - offset {
            return None;
        }
        if chunks == 1 && kind != b"JSON" {
            return None;
        }
        match kind {
            b"JSON" => {
                if json.is_some() || size > MAX_JSON_BYTES {
                    return None;
                }
                json = Some(read_bytes(reader, size)?);
            }
            b"BIN\0" => {
                if bin.is_some() || chunks != 2 || size > MAX_BUFFER_BYTES {
                    return None;
                }
                bin = Some((offset, size));
            }
            _ => {}
        }
        offset += size as u64;
        reader.seek(SeekFrom::Start(offset)).ok()?;
    }
    Some(Container { json: json?, bin })
}

/// Only relative companion files inside the document directory are opened.
/// URI schemes, absolute paths, traversal and symlinks escaping it are refused.
fn local_resource(root: &Path, uri: &str) -> Option<PathBuf> {
    let decoded = super::model::decode_uri_path(uri)?;
    if decoded.is_empty()
        || decoded.starts_with('/')
        || decoded
            .chars()
            .any(|ch| ch.is_control() || "\\:*?<>|#".contains(ch))
    {
        return None;
    }
    let relative = Path::new(&decoded);
    if relative.components().any(|part| {
        !matches!(
            part,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return None;
    }
    let candidate = root.join(relative);
    if !crate::fs::content_is_local(&candidate) {
        return None;
    }
    let resolved = candidate.canonicalize().ok()?;
    resolved.starts_with(root).then_some(resolved)
}

fn data_uri(uri: &str, expected: usize) -> Option<Vec<u8>> {
    let (header, data) = uri.split_once(',')?;
    if !matches!(
        header,
        "data:application/octet-stream;base64" | "data:application/gltf-buffer;base64"
    ) {
        return None;
    }
    if expected > MAX_BUFFER_BYTES || data.len() != expected.div_ceil(3).checked_mul(4)? {
        return None;
    }
    let digit = |byte| match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut result = Vec::with_capacity(expected);
    for (index, block) in data.as_bytes().chunks_exact(4).enumerate() {
        let last = (index + 1) * 4 == data.len();
        let a = digit(block[0])?;
        let b = digit(block[1])?;
        result.push(a << 2 | b >> 4);
        if block[2] == b'=' {
            if !last || block[3] != b'=' || b & 15 != 0 {
                return None;
            }
        } else {
            let c = digit(block[2])?;
            result.push(b << 4 | c >> 2);
            if block[3] == b'=' {
                if !last || c & 3 != 0 {
                    return None;
                }
            } else {
                result.push(c << 6 | digit(block[3])?);
            }
        }
    }
    (result.len() == expected).then_some(result)
}

fn multiply(a: Matrix, b: Matrix) -> Matrix {
    std::array::from_fn(|i| (0..4).map(|k| a[k * 4 + i % 4] * b[i / 4 * 4 + k]).sum())
}

fn local_transform(node: &Node) -> Option<Matrix> {
    if let Some(matrix) = node.matrix {
        if node.translation.is_some()
            || node.rotation.is_some()
            || node.scale.is_some()
            || !matrix.iter().all(|v| v.is_finite())
            || matrix[3] != 0.0
            || matrix[7] != 0.0
            || matrix[11] != 0.0
            || matrix[15] != 1.0
        {
            return None;
        }
        return Some(matrix);
    }
    let [x, y, z, w] = node.rotation.unwrap_or([0., 0., 0., 1.]);
    let norm = x * x + y * y + z * z + w * w;
    if !norm.is_finite() || (norm - 1.0).abs() > 0.001 {
        return None;
    }
    let mut matrix = [
        1. - 2. * (y * y + z * z),
        2. * (x * y + z * w),
        2. * (x * z - y * w),
        0.,
        2. * (x * y - z * w),
        1. - 2. * (x * x + z * z),
        2. * (y * z + x * w),
        0.,
        2. * (x * z + y * w),
        2. * (y * z - x * w),
        1. - 2. * (x * x + y * y),
        0.,
        0.,
        0.,
        0.,
        1.,
    ];
    for (axis, scale) in node.scale.unwrap_or([1.; 3]).into_iter().enumerate() {
        for row in 0..3 {
            matrix[axis * 4 + row] *= scale;
        }
    }
    matrix[12..15].copy_from_slice(&node.translation.unwrap_or([0.; 3]));
    matrix.iter().all(|v| v.is_finite()).then_some(matrix)
}

fn instances(doc: &Document, deadline: Instant) -> Option<Vec<(usize, Matrix)>> {
    fn walk(
        doc: &Document,
        index: usize,
        parent: Matrix,
        seen: &mut [bool],
        output: &mut Vec<(usize, Matrix)>,
        depth: usize,
        deadline: Instant,
    ) -> Option<()> {
        if depth > 64 || Instant::now() >= deadline || *seen.get(index)? {
            return None;
        }
        seen[index] = true;
        let node = doc.nodes.get(index)?;
        if node.skin.is_some() {
            return None;
        }
        let matrix = multiply(parent, local_transform(node)?);
        if !matrix.iter().all(|v| v.is_finite()) {
            return None;
        }
        if let Some(mesh) = node.mesh {
            doc.meshes.get(mesh)?;
            output.push((mesh, matrix));
        }
        for &child in &node.children {
            walk(doc, child, matrix, seen, output, depth + 1, deadline)?;
        }
        Some(())
    }
    let roots = if !doc.scenes.is_empty() {
        doc.scenes.get(doc.scene.unwrap_or(0))?.nodes.clone()
    } else {
        if doc.scene.is_some() {
            return None;
        }
        let mut child = vec![false; doc.nodes.len()];
        for node in &doc.nodes {
            for &index in &node.children {
                *child.get_mut(index)? = true;
            }
        }
        child
            .iter()
            .enumerate()
            .filter_map(|(i, child)| (!child).then_some(i))
            .collect()
    };
    let mut output = Vec::new();
    let mut seen = vec![false; doc.nodes.len()];
    for root in roots {
        walk(doc, root, IDENTITY, &mut seen, &mut output, 0, deadline)?;
    }
    (!output.is_empty()).then_some(output)
}

struct Span<'a> {
    bytes: &'a [u8],
    stride: usize,
    width: usize,
}
impl<'a> Span<'a> {
    fn element(&self, index: usize) -> &'a [u8] {
        &self.bytes[index * self.stride..index * self.stride + self.width]
    }
}

fn span<'a>(
    doc: &Document,
    buffers: &'a [Vec<u8>],
    view: usize,
    offset: usize,
    (width, alignment): (usize, usize),
    count: usize,
    strided: bool,
) -> Option<Span<'a>> {
    let view = doc.buffer_views.get(view)?;
    let buffer = buffers.get(view.buffer)?;
    let end = view.byte_offset.checked_add(view.byte_length)?;
    let bytes = buffer.get(view.byte_offset..end)?;
    if count == 0
        || count > MAX_BUFFER_BYTES
        || !offset.is_multiple_of(alignment)
        || !view.byte_offset.is_multiple_of(alignment)
    {
        return None;
    }
    if view.byte_stride.is_some() && !strided {
        return None;
    }
    let stride = view.byte_stride.unwrap_or(width);
    if stride < width
        || !stride.is_multiple_of(alignment)
        || (view.byte_stride.is_some() && (stride > 252 || !stride.is_multiple_of(4)))
    {
        return None;
    }
    let size = (count - 1).checked_mul(stride)?.checked_add(width)?;
    Some(Span {
        bytes: bytes.get(offset..offset.checked_add(size)?)?,
        stride,
        width,
    })
}

struct Data<'a> {
    base: Option<Span<'a>>,
    sparse: Option<(Span<'a>, Span<'a>, usize)>,
    count: usize,
    width: usize,
}

fn index_width(component: u32) -> Option<usize> {
    match component {
        5121 => Some(1),
        5123 => Some(2),
        5125 => Some(4),
        _ => None,
    }
}

fn unsigned(bytes: &[u8]) -> usize {
    match bytes.len() {
        1 => bytes[0] as usize,
        2 => u16::from_le_bytes([bytes[0], bytes[1]]) as usize,
        4 => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize,
        _ => unreachable!(),
    }
}

impl<'a> Data<'a> {
    fn new(
        doc: &Document,
        buffers: &'a [Vec<u8>],
        index: usize,
        position: bool,
        deadline: Instant,
    ) -> Option<Self> {
        let accessor = doc.accessors.get(index)?;
        if accessor.normalized || accessor.count == 0 {
            return None;
        }
        let width = if position {
            if accessor.kind != "VEC3" || accessor.component_type != 5126 {
                return None;
            }
            12
        } else {
            if accessor.kind != "SCALAR" {
                return None;
            }
            index_width(accessor.component_type)?
        };
        let alignment = if position { 4 } else { width };
        let base = if let Some(view) = accessor.buffer_view {
            Some(span(
                doc,
                buffers,
                view,
                accessor.byte_offset,
                (width, alignment),
                accessor.count,
                position,
            )?)
        } else {
            if accessor.byte_offset != 0 || accessor.count > MAX_BUFFER_BYTES / width {
                return None;
            }
            None
        };
        let sparse = if let Some(sparse) = &accessor.sparse {
            if sparse.count > accessor.count {
                return None;
            }
            let iw = index_width(sparse.indices.component_type)?;
            let indices = span(
                doc,
                buffers,
                sparse.indices.buffer_view,
                sparse.indices.byte_offset,
                (iw, iw),
                sparse.count,
                false,
            )?;
            let values = span(
                doc,
                buffers,
                sparse.values.buffer_view,
                sparse.values.byte_offset,
                (width, alignment),
                sparse.count,
                false,
            )?;
            let mut previous = None;
            for i in 0..sparse.count {
                if i % 1024 == 0 && Instant::now() >= deadline {
                    return None;
                }
                let current = unsigned(indices.element(i));
                if current >= accessor.count || previous.is_some_and(|p| current <= p) {
                    return None;
                }
                previous = Some(current);
            }
            Some((indices, values, sparse.count))
        } else {
            None
        };
        Some(Self {
            base,
            sparse,
            count: accessor.count,
            width,
        })
    }

    fn element(&self, index: usize) -> Option<&[u8]> {
        if index >= self.count {
            return None;
        }
        if let Some((indices, values, count)) = &self.sparse {
            let mut low = 0;
            let mut high = *count;
            while low < high {
                let mid = low + (high - low) / 2;
                if unsigned(indices.element(mid)) < index {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            if low < *count && unsigned(indices.element(low)) == index {
                return Some(values.element(low));
            }
        }
        Some(
            self.base
                .as_ref()
                .map_or(&[0; 12][..self.width], |base| base.element(index)),
        )
    }
}

fn visit(
    doc: &Document,
    buffers: &[Vec<u8>],
    instances: &[(usize, Matrix)],
    deadline: Instant,
    consume: &mut mesh::Consumer<'_>,
) -> Option<()> {
    for &(mesh, matrix) in instances {
        for primitive in &doc.meshes.get(mesh)?.primitives {
            if Instant::now() >= deadline || !primitive.targets.is_empty() {
                return None;
            }
            let mode = primitive.mode.unwrap_or(4);
            if mode < 4 {
                continue;
            }
            if mode > 6 {
                return None;
            }
            let positions =
                Data::new(doc, buffers, primitive.attributes.position?, true, deadline)?;
            let indices = match primitive.indices {
                Some(index) => Some(Data::new(doc, buffers, index, false, deadline)?),
                None => None,
            };
            let count = indices.as_ref().map_or(positions.count, |data| data.count);
            if count < 3 || (mode == 4 && count % 3 != 0) {
                return None;
            }
            let triangles = if mode == 4 { count / 3 } else { count - 2 };
            if triangles as u64 > mesh::MAX_TRIANGLES {
                return None;
            }
            for i in 0..triangles {
                let corners = match mode {
                    4 => [i * 3, i * 3 + 1, i * 3 + 2],
                    5 if i % 2 == 0 => [i, i + 1, i + 2],
                    5 => [i + 1, i, i + 2],
                    _ => [0, i + 1, i + 2],
                };
                let mut triangle = [[0f32; 3]; 3];
                for (vertex, index) in triangle.iter_mut().zip(corners) {
                    let index = if let Some(indices) = &indices {
                        unsigned(indices.element(index)?)
                    } else {
                        index
                    };
                    let bytes = positions.element(index)?;
                    let p: [f64; 3] = std::array::from_fn(|axis| {
                        f64::from(f32::from_le_bytes(
                            bytes[axis * 4..axis * 4 + 4].try_into().unwrap(),
                        ))
                    });
                    let world = std::array::from_fn(|axis| {
                        (matrix[12 + axis]
                            + (0..3).map(|k| matrix[k * 4 + axis] * p[k]).sum::<f64>())
                            as f32
                    });
                    *vertex = mesh::y_up(world);
                }
                consume(triangle)?;
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample() -> (Value, Vec<u8>) {
        let points = [[0f32, 0., 0.], [1., 0., 0.], [0., 1., 0.]];
        let bytes = points
            .iter()
            .flatten()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        (
            json!({
                "asset": {"version": "2.0"}, "scene": 0, "scenes": [{"nodes": [0]}],
                "nodes": [{"mesh": 0}], "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
                "accessors": [{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3"}],
                "bufferViews": [{"buffer": 0, "byteLength": 36}], "buffers": [{"byteLength": 36}]
            }),
            bytes,
        )
    }

    fn parsed(value: &Value) -> Option<Document> {
        document(&serde_json::to_vec(value).unwrap())
    }

    fn triangles(value: &Value, bytes: Vec<u8>) -> Option<Vec<mesh::Triangle>> {
        let doc = parsed(value)?;
        let deadline = Instant::now() + mesh::TIME_LIMIT;
        let instances = instances(&doc, deadline)?;
        let mut output = Vec::new();
        visit(&doc, &[bytes], &instances, deadline, &mut |triangle| {
            output.push(triangle);
            Some(())
        })?;
        Some(output)
    }

    fn glb(value: &Value, payload: &[u8]) -> Vec<u8> {
        let mut json = serde_json::to_vec(value).unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut bin = payload.to_vec();
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let mut bytes = b"glTF".to_vec();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&((28 + json.len() + bin.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"JSON");
        bytes.extend(json);
        bytes.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"BIN\0");
        bytes.extend(bin);
        bytes
    }

    struct Files(PathBuf);
    impl Files {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target")
                .join(format!(
                    "model-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Files {
        fn drop(&mut self) {
            for name in ["my model.bin", "model.gltf", "model.glb"] {
                let _ = std::fs::remove_file(self.0.join(name));
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn binary_and_external_json_assets_render_identically() {
        let files = Files::new();
        let (mut value, bytes) = sample();
        let binary = files.write("model.glb", &glb(&value, &bytes));
        let expected = from_gltf(&binary, 64).unwrap();
        files.write("my model.bin", &bytes);
        value["buffers"][0]["uri"] = json!("my%20model.bin");
        let text = files.write("model.gltf", &serde_json::to_vec(&value).unwrap());
        assert_eq!(from_gltf(&text, 64), Some(expected));
        std::fs::remove_file(files.0.join("my model.bin")).unwrap();
        assert!(from_gltf(&text, 64).is_none());
        assert!(from_gltf(&binary, 0).is_none());
    }

    #[test]
    fn buffer_uris_are_local_and_percent_decoded_once() {
        let files = Files::new();
        let path = files.write("my model.bin", &[1, 2, 3]);
        assert_eq!(local_resource(&files.0, "my%20model.bin"), Some(path));
        for uri in [
            "../my%20model.bin",
            "%2e%2e/my.bin",
            "/my.bin",
            "C:/my.bin",
            "file:my.bin",
            "remote:my.bin",
            "//host/my.bin",
            "%5c%5chost/my.bin",
            "my%00.bin",
            "my.bin#fragment",
            "my.bin?query",
            "my%2.bin",
            "my%2520model.bin",
        ] {
            assert!(local_resource(&files.0, uri).is_none(), "{uri}");
        }
        #[cfg(unix)]
        {
            let outside = Files::new();
            outside.write("my model.bin", &[1]);
            std::os::unix::fs::symlink(&outside.0, files.0.join("link")).unwrap();
            assert!(local_resource(&files.0, "link/my%20model.bin").is_none());
            std::fs::remove_file(files.0.join("link")).unwrap();
        }
    }

    #[test]
    fn data_uris_validate_padding_lengths_and_media_type() {
        for (encoded, bytes) in [
            ("AA==", vec![0]),
            ("AAE=", vec![0, 1]),
            ("AAEC", vec![0, 1, 2]),
            ("AAECAwQF", vec![0, 1, 2, 3, 4, 5]),
        ] {
            assert_eq!(
                data_uri(
                    &format!("data:application/octet-stream;base64,{encoded}"),
                    bytes.len()
                ),
                Some(bytes)
            );
        }
        for encoded in ["AB==", "AAF=", "AA=A", "AA==AA==", "AA?=", "A==="] {
            assert!(
                data_uri(
                    &format!("data:application/octet-stream;base64,{encoded}"),
                    1
                )
                .is_none()
            );
        }
        assert!(data_uri("data:text/plain;base64,AA==", 1).is_none());
        assert!(
            data_uri(
                "data:application/octet-stream;base64,AA==",
                MAX_BUFFER_BYTES + 1
            )
            .is_none()
        );
    }

    #[test]
    fn indices_support_all_unsigned_widths_and_triangle_topologies() {
        for (component, width) in [(5121, 1), (5123, 2), (5125, 4)] {
            for mode in [4, 5, 6] {
                let (mut value, mut bytes) = sample();
                for index in [0u32, 1, 2] {
                    bytes.extend_from_slice(&index.to_le_bytes()[..width]);
                }
                value["bufferViews"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"buffer": 0, "byteOffset": 36, "byteLength": width * 3}));
                value["accessors"].as_array_mut().unwrap().push(json!({"bufferView": 1, "componentType": component, "count": 3, "type": "SCALAR"}));
                value["meshes"][0]["primitives"][0]["indices"] = json!(1);
                value["meshes"][0]["primitives"][0]["mode"] = json!(mode);
                assert_eq!(triangles(&value, bytes).unwrap().len(), 1);
            }
        }
        let (mut value, mut bytes) = sample();
        bytes.extend([1f32, 1., 0.].iter().flat_map(|v| v.to_le_bytes()));
        value["bufferViews"][0]["byteLength"] = json!(48);
        value["accessors"][0]["count"] = json!(4);
        for mode in [5, 6] {
            value["meshes"][0]["primitives"][0]["mode"] = json!(mode);
            let result = triangles(&value, bytes.clone()).unwrap();
            assert_eq!(result.len(), 2);
            assert_eq!(result[1][0], result[0][if mode == 5 { 2 } else { 0 }]);
        }
    }

    #[test]
    fn interleaved_and_sparse_positions_match_packed_geometry() {
        let (mut value, bytes) = sample();
        let expected = triangles(&value, bytes.clone()).unwrap();
        let mut interleaved = vec![0; 8];
        for position in bytes.chunks_exact(12) {
            interleaved.extend_from_slice(position);
            interleaved.extend_from_slice(&[0; 4]);
        }
        value["bufferViews"][0] =
            json!({"buffer": 0, "byteOffset": 4, "byteLength": 52, "byteStride": 16});
        value["accessors"][0]["byteOffset"] = json!(4);
        assert_eq!(triangles(&value, interleaved), Some(expected.clone()));
        let (mut value, _) = sample();
        value["bufferViews"] = json!([{"buffer": 0, "byteLength": 3}, {"buffer": 0, "byteOffset": 4, "byteLength": 36}]);
        value["accessors"][0]
            .as_object_mut()
            .unwrap()
            .remove("bufferView");
        value["accessors"][0]["sparse"] = json!({"count": 3, "indices": {"bufferView": 0, "componentType": 5121}, "values": {"bufferView": 1}});
        let mut sparse = vec![0, 1, 2, 0];
        sparse.extend(bytes);
        assert_eq!(triangles(&value, sparse.clone()), Some(expected));
        sparse[1] = 0;
        assert!(triangles(&value, sparse.clone()).is_none());
        sparse[1] = 1;
        sparse[2] = 3;
        assert!(triangles(&value, sparse).is_none());
    }

    #[test]
    fn selected_scene_composes_parent_and_child_transforms() {
        let (mut value, bytes) = sample();
        value["nodes"] = json!([
            {"translation": [1,2,3], "children": [1]},
            {"mesh": 0, "scale": [2,3,4], "rotation": [0,0,std::f64::consts::FRAC_1_SQRT_2,std::f64::consts::FRAC_1_SQRT_2]},
            {"mesh": 999}
        ]);
        let actual = triangles(&value, bytes.clone()).unwrap()[0];
        let expected = [[1f32, -3., 2.], [1., -3., 4.], [-2., -3., 2.]];
        for (a, b) in actual.iter().flatten().zip(expected.iter().flatten()) {
            assert!((a - b).abs() < 1e-5);
        }
        let doc = parsed(&value).unwrap();
        let matrix = local_transform(&doc.nodes[1]).unwrap();
        value["nodes"][1] = json!({"mesh": 0, "matrix": matrix});
        assert_eq!(triangles(&value, bytes), Some(vec![actual]));
        value["nodes"][1]["translation"] = json!([0, 0, 0]);
        assert!(instances(&parsed(&value).unwrap(), Instant::now() + mesh::TIME_LIMIT).is_none());
    }

    #[test]
    fn malformed_accessors_and_scene_graphs_fail_without_panicking() {
        let (value, bytes) = sample();
        for (pointer, replacement) in [
            ("/accessors/0/count", json!(u64::MAX)),
            ("/accessors/0/componentType", json!(5122)),
            ("/accessors/0/type", json!("VEC4")),
            ("/accessors/0/bufferView", json!(99)),
            ("/bufferViews/0/byteLength", json!(u64::MAX)),
            ("/bufferViews/0/buffer", json!(99)),
        ] {
            let mut bad = value.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            assert!(triangles(&bad, bytes.clone()).is_none(), "{pointer}");
        }
        for (key, replacement) in [("byteOffset", json!(u64::MAX)), ("normalized", json!(true))] {
            let mut bad = value.clone();
            bad["accessors"][0][key] = replacement;
            assert!(triangles(&bad, bytes.clone()).is_none());
        }
        for stride in [1, 13, 256] {
            let mut bad = value.clone();
            bad["bufferViews"][0]["byteStride"] = json!(stride);
            assert!(triangles(&bad, bytes.clone()).is_none());
        }
        for node in [
            json!({"mesh": 0, "children": [0]}),
            json!({"mesh": 0, "children": [99]}),
            json!({"mesh": 0, "skin": 0}),
            json!({"mesh": 0, "rotation": [0,0,0,0]}),
        ] {
            let mut bad = value.clone();
            bad["nodes"][0] = node;
            assert!(triangles(&bad, bytes.clone()).is_none());
        }
        let mut bad = value.clone();
        bad["meshes"][0]["primitives"][0]["targets"] = json!([{"POSITION":0}]);
        assert!(triangles(&bad, bytes).is_none());
    }

    #[test]
    fn unsupported_versions_extensions_and_oversized_arrays_are_refused() {
        let (mut value, _) = sample();
        value["asset"]["version"] = json!("1.0");
        assert!(parsed(&value).is_none());
        value["asset"]["version"] = json!("2.0");
        value["extensionsRequired"] = json!(["KHR_draco_mesh_compression"]);
        assert!(parsed(&value).is_none());
        value["extensionsRequired"] = json!([]);
        value["nodes"] = json!(vec![json!({}); MAX_NODES + 1]);
        assert!(parsed(&value).is_none());
        value["nodes"] = json!([]);
        value["scenes"] = json!(vec![json!({"nodes":[]}); MAX_ENTRIES + 1]);
        assert!(parsed(&value).is_none());
        assert!(document(&vec![b' '; MAX_JSON_BYTES + 1]).is_none());
    }

    #[test]
    fn glb_chunk_sizes_order_and_truncation_are_validated() {
        let (value, bytes) = sample();
        let valid = glb(&value, &bytes);
        let read = |bytes: &[u8]| container(&mut Cursor::new(bytes), bytes.len() as u64);
        assert!(read(&valid).is_some());
        for end in [0, 4, 11, 20, valid.len() - 1] {
            assert!(read(&valid[..end]).is_none());
        }
        for (offset, replacement) in [
            (4, 1u32.to_le_bytes()),
            (8, 0u32.to_le_bytes()),
            (12, u32::MAX.to_le_bytes()),
            (16, *b"BIN\0"),
        ] {
            let mut invalid = valid.clone();
            invalid[offset..offset + 4].copy_from_slice(&replacement);
            assert!(read(&invalid).is_none());
        }
    }
}
