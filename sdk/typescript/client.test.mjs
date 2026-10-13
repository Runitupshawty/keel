import { test } from 'node:test';
import assert from 'node:assert/strict';
import { KeelClient, RpcError } from './dist/keel.js';

test('methods send JSON-RPC and preserve preview results', async () => {
  const requests = [];
  const preview = { plan_id: 'plan', input_hash: 'hash' };
  const client = new KeelClient(async request => {
    requests.push(request);
    return { jsonrpc: '2.0', id: request.id, result: preview };
  });
  assert.equal(await client['tags.set']({ paths: ['/files/a'], tags: ['tag'] }), preview);
  await client.call('library.stats', {});
  assert.deepEqual(requests[0], {
    jsonrpc: '2.0', id: 1, method: 'tags.set', params: { paths: ['/files/a'], tags: ['tag'] }
  });
  assert.equal(requests[1].id, 2);
});

test('RPC errors preserve code and data', async () => {
  const client = new KeelClient(async r => ({ jsonrpc: '2.0', id: r.id,
    error: { code: -32001, message: 'expired', data: { fresh: true } } }));
  await assert.rejects(client['library.stats']({}), e =>
    e instanceof RpcError && e.code === -32001 && e.data.fresh === true);
});

test('mismatched responses and absent results are refused; null is a result', async () => {
  for (const reply of [{ jsonrpc: '2.0', id: 999, result: {} }, { jsonrpc: '2.0', id: 1 }]) {
    const client = new KeelClient(async () => reply);
    await assert.rejects(client['library.stats']({}));
  }
  const client = new KeelClient(async r => ({ jsonrpc: '2.0', id: r.id, result: null }));
  assert.equal(await client['library.stats']({}), null);
});
