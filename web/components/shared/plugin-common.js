// Shared helpers for the plugin pages (`plugin-catalog`, `plugin-detail`).
// Kept separate from `connector-common.js` on purpose: the plugin model
// (JSON-Schema config blobs, `plugin_access`) is not the connector model
// (env/api_key manifests).

export async function jf(url, opts) {
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error(await res.text() || `HTTP ${res.status}`);
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : null;
}

/// Normalizes a plugin JSON Schema (`{properties, required}`) into flat field
/// descriptors. Only the scalar types a form can render are supported.
export function schemaFields(schema) {
  const props = schema?.properties;
  if (!props || typeof props !== 'object') return [];
  const required = new Set(Array.isArray(schema.required) ? schema.required : []);
  return Object.entries(props).map(([key, p]) => ({
    key,
    label:       p.title || key,
    description: p.description || '',
    type:        p.type === 'boolean' ? 'boolean' : (p.type === 'integer' || p.type === 'number') ? 'number' : 'string',
    required:    required.has(key),
    sensitive:   !!p.sensitive,
  }));
}

export const hasSchema = (schema) => schemaFields(schema).length > 0;

/// Required config keys with no persisted value yet.
export function missingRequired(p) {
  return schemaFields(p.config_schema)
    .filter(f => f.required && (p.config?.[f.key] === undefined || p.config?.[f.key] === null || p.config?.[f.key] === ''));
}

/// Admin-catalog health of a plugin: 'off' | 'needs_config' | 'not_running' | 'ok'.
/// Green only when enabled, running and every required config key is set.
export function pluginHealth(p) {
  if (!p.enabled) return 'off';
  if (missingRequired(p).length) return 'needs_config';
  if (!p.running) return 'not_running';
  return 'ok';
}
