import { beforeEach, describe, expect, it, vi } from 'vitest';
import { CodeIntelClient } from './client';

describe('CodeIntelClient.listTools', () => {
  const fetchMock = vi.fn();
  const client = new CodeIntelClient('http://example.test');

  beforeEach(() => {
    fetchMock.mockReset();
    vi.stubGlobal('fetch', fetchMock);
  });

  it('returns rich tool metadata from the HTTP API', async () => {
    fetchMock.mockResolvedValue({
      ok: true,
      json: async () => ({
        tools: [
          {
            name: 'list_repos',
            description: 'List repositories',
            category: 'Repository',
            stability: 'Stable',
            performance: 'Low',
            requires_api_key: false,
            required_flags: [],
            tags: ['repository', 'list'],
            aliases: ['repos'],
            input_schema: { type: 'object', properties: {}, required: [] },
            annotations: {
              title: 'list repos',
              readOnlyHint: true,
              destructiveHint: false,
              idempotentHint: true,
              openWorldHint: false,
            },
          },
        ],
      }),
    });

    const tools = await client.listTools();

    expect(fetchMock).toHaveBeenCalledWith('http://example.test/tools');
    expect(tools).toHaveLength(1);
    expect(tools[0]).toMatchObject({
      name: 'list_repos',
      category: 'Repository',
      tags: ['repository', 'list'],
      annotations: {
        readOnlyHint: true,
        idempotentHint: true,
      },
    });
  });

  it('throws on unsuccessful responses', async () => {
    fetchMock.mockResolvedValue({
      ok: false,
      statusText: 'Bad Gateway',
    });

    await expect(client.listTools()).rejects.toThrow('Failed to list tools: Bad Gateway');
  });
});
