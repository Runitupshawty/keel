import { KeelClient, type Operations } from './keel.js';

declare const client: KeelClient;
const preview: Promise<Operations['tags.set']['result']> = client['tags.set']({
  paths: ['/files/a'], tags: ['tag']
});
void preview;
// Operations added since the original SDK's registry.
const names: Promise<{ paths: string[]; truncated: boolean }> = client['files.names']({ max: 10 });
const bookmark: Promise<Operations['views.create']['result']> = client['views.create']({
  name: 'Invoices', query: 'invoice', backend: 'library', columns: ['name', 'size']
});
void names;
void bookmark;
client['library.set_folder_style']({ path: '/files', color: '#3b82f6' });
client['library.set_comment']({ path: '/files/a', comment: 'Review' });
client['du.result']({ id: 'scan', wait_ms: 100 });
// @ts-expect-error file name limit must be a number
client['files.names']({ max: '10' });
// @ts-expect-error unknown operation
client.call('missing.operation', {});
// @ts-expect-error tags must be strings
client['tags.set']({ paths: ['/files/a'], tags: [1] });
// @ts-expect-error mandatory input is missing
client['tags.set']({});
