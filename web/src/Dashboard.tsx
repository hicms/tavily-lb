import {useCallback, useEffect, useMemo, useState} from 'react';
import {AlertDialog} from '@astryxdesign/core/AlertDialog';
import {Badge} from '@astryxdesign/core/Badge';
import {Banner} from '@astryxdesign/core/Banner';
import {Button} from '@astryxdesign/core/Button';
import {Card} from '@astryxdesign/core/Card';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {Grid} from '@astryxdesign/core/Grid';
import {HStack, VStack} from '@astryxdesign/core/Layout';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {ProgressBar} from '@astryxdesign/core/ProgressBar';
import {Table, pixel, proportional, type TableColumn} from '@astryxdesign/core/Table';
import {Heading, Text} from '@astryxdesign/core/Text';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {useToast} from '@astryxdesign/core/Toast';
import {TopNav} from '@astryxdesign/core/TopNav';
import {
  api,
  errorMessage,
  type ImportResult,
  type KeyItem,
  type KeyStatus,
  type Overview,
  type Settings,
  type SyncStatus,
} from './api';
import {EditDialog, ImportDialog} from './dialogs';

const REFRESH_MS = 10_000;
const SYNC_POLL_MS = 1_500;

const STATUS_BADGE: Record<KeyStatus, {label: string; variant: 'success' | 'warning' | 'error' | 'neutral' | 'info'}> = {
  available: {label: '可用', variant: 'success'},
  cooling: {label: '冷却中', variant: 'warning'},
  exhausted: {label: '已耗尽', variant: 'info'},
  disabled: {label: '已禁用', variant: 'neutral'},
  invalid: {label: '失效', variant: 'error'},
};

const numberFmt = new Intl.NumberFormat('zh-CN');
const fmt = (n: number) => numberFmt.format(n);

interface Confirm {
  title: string;
  description: string;
  actionLabel: string;
  run: () => Promise<void>;
}

export function Dashboard({onLogout}: {onLogout: () => void}) {
  const toast = useToast();
  const [overview, setOverview] = useState<Overview | null>(null);
  const [keys, setKeys] = useState<KeyItem[] | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [sync, setSync] = useState<SyncStatus | null>(null);
  const [loadError, setLoadError] = useState('');
  const [importOpen, setImportOpen] = useState(false);
  const [editing, setEditing] = useState<KeyItem | null>(null);
  const [confirm, setConfirm] = useState<Confirm | null>(null);
  const [confirmLoading, setConfirmLoading] = useState(false);

  const notifyError = useCallback(
    (e: unknown) => toast({body: errorMessage(e), type: 'error'}),
    [toast],
  );

  const refresh = useCallback(async () => {
    try {
      const [o, k] = await Promise.all([api.overview(), api.listKeys()]);
      setOverview(o);
      setKeys(k);
      setLoadError('');
    } catch (e) {
      setLoadError(errorMessage(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    api.settings().then(setSettings).catch(() => {});
    api.syncStatus().then(setSync).catch(() => {});
    const timer = window.setInterval(() => void refresh(), REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const syncRunning = sync?.running ?? false;
  useEffect(() => {
    if (!syncRunning) return;
    const timer = window.setInterval(async () => {
      try {
        const s = await api.syncStatus();
        setSync(s);
        if (!s.running) {
          void refresh();
          toast({
            body: `用量同步完成：成功 ${s.succeeded}，失败 ${s.failed}${s.skipped ? `，因 Tavily 限流跳过 ${s.skipped}` : ''}`,
            type: s.failed > 0 ? 'error' : 'info',
          });
        }
      } catch {
        // The next tick retries.
      }
    }, SYNC_POLL_MS);
    return () => window.clearInterval(timer);
  }, [syncRunning, refresh, toast]);

  const replaceKey = useCallback((item: KeyItem) => {
    setKeys((prev) => prev?.map((k) => (k.id === item.id ? item : k)) ?? prev);
    api.overview().then(setOverview).catch(() => {});
  }, []);

  const runConfirm = async () => {
    if (!confirm) return;
    setConfirmLoading(true);
    try {
      await confirm.run();
      setConfirm(null);
    } catch (e) {
      notifyError(e);
    } finally {
      setConfirmLoading(false);
    }
  };

  const startSync = async () => {
    try {
      const {started, status} = await api.startSync();
      setSync(status);
      toast({body: started ? '已开始同步全部 Key 的用量' : '已有同步任务在运行'});
    } catch (e) {
      notifyError(e);
    }
  };

  const exportKeys = async () => {
    try {
      const blob = await api.exportKeys();
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = 'tavily-keys.txt';
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      notifyError(e);
    }
  };

  const onImported = (r: ImportResult) => {
    toast({body: `导入 ${r.added} 个，重复 ${r.duplicates} 个${r.errors.length ? `，错误 ${r.errors.length} 行` : ''}`});
    void refresh();
  };

  const columns = useMemo<TableColumn<KeyItem>[]>(
    () => [
      {
        key: 'alias',
        header: 'Key',
        width: proportional(2),
        renderCell: (k) => (
          <VStack gap={0.5}>
            <Text type="body" weight="medium">
              {k.alias || k.key}
            </Text>
            <Text type="code" size="sm" color="secondary">
              {k.key}
            </Text>
          </VStack>
        ),
      },
      {
        key: 'status',
        header: '状态',
        width: pixel(130),
        renderCell: (k) => {
          const b = STATUS_BADGE[k.status];
          const label = k.status === 'cooling' && k.cooldown_seconds > 0 ? `${b.label} ${k.cooldown_seconds}s` : b.label;
          return <Badge variant={b.variant} label={label} />;
        },
      },
      {
        key: 'used_quota',
        header: '额度（已用 / 总计）',
        width: proportional(2),
        renderCell: (k) => (
          <VStack gap={1}>
            <Text type="body" size="sm">
              {fmt(k.used_quota)} / {fmt(k.total_quota)}
            </Text>
            <ProgressBar
              label={`${k.alias} 额度使用`}
              isLabelHidden
              value={Math.min(k.used_quota, k.total_quota)}
              max={Math.max(k.total_quota, 1)}
              variant={usageVariant(k.used_quota, k.total_quota)}
            />
          </VStack>
        ),
      },
      {
        key: 'remaining',
        header: '剩余',
        width: pixel(90),
        align: 'end',
        renderCell: (k) => <Text type="body">{fmt(k.remaining)}</Text>,
      },
      {
        key: 'last_used_at',
        header: '最近使用',
        width: pixel(130),
        renderCell: (k) =>
          k.last_used_at ? (
            <Timestamp value={k.last_used_at} format="relative" />
          ) : (
            <Text type="supporting" color="secondary">
              从未
            </Text>
          ),
      },
      {
        key: 'id',
        header: '',
        width: pixel(64),
        align: 'end',
        renderCell: (k) => (
          <MoreMenu
            label={`${k.alias} 的操作`}
            alignment="end"
            items={[
              {
                label: '复制完整 Key',
                onClick: async () => {
                  try {
                    await navigator.clipboard.writeText(await api.rawKey(k.id));
                    toast({body: '已复制到剪贴板'});
                  } catch (e) {
                    notifyError(e);
                  }
                },
              },
              {label: '编辑', onClick: () => setEditing(k)},
              {
                label: k.active ? '禁用' : '启用',
                onClick: async () => {
                  try {
                    replaceKey(await api.updateKey(k.id, {active: !k.active}));
                  } catch (e) {
                    notifyError(e);
                  }
                },
              },
              {
                label: '同步用量',
                onClick: async () => {
                  try {
                    replaceKey(await api.syncKey(k.id));
                    toast({body: `${k.alias} 用量已同步`});
                  } catch (e) {
                    notifyError(e);
                    void refresh();
                  }
                },
              },
              {
                label: '重置已用额度',
                onClick: () =>
                  setConfirm({
                    title: '重置已用额度？',
                    description: `将 ${k.alias} 的已用额度清零并清除冷却状态。`,
                    actionLabel: '重置',
                    run: async () => replaceKey(await api.resetKey(k.id)),
                  }),
              },
              {type: 'divider'},
              {
                label: '删除',
                variant: 'destructive',
                onClick: () =>
                  setConfirm({
                    title: '删除 Key？',
                    description: `将从池中永久移除 ${k.alias}（${k.key}）。`,
                    actionLabel: '删除',
                    run: async () => {
                      await api.deleteKey(k.id);
                      toast({body: '已删除'});
                      await refresh();
                    },
                  }),
              },
            ]}
          />
        ),
      },
    ],
    [notifyError, refresh, replaceKey, toast],
  );

  const defaultQuota = Number(settings?.config.default_total_quota ?? 1000) || 1000;

  return (
    <VStack gap={0} minHeight="100%">
      <TopNav
        heading={
          <Text type="body" weight="bold">
            Tavily LB
          </Text>
        }
        endContent={
          <HStack gap={2} vAlign="center">
            {settings && <Badge label={`v${settings.version}`} />}
            <Button label="退出登录" variant="ghost" size="sm" onClick={onLogout} />
          </HStack>
        }
      />
      <VStack gap={6} padding={6} width="100%" maxWidth={1280} style={{marginInline: 'auto'}}>
        {settings?.master_key_source === 'generated' && (
          <Banner
            status="warning"
            title="Master Key 为自动生成"
            description="建议在配置文件或 MASTER_KEY 环境变量中设置固定的 Master Key。"
            isDismissable
          />
        )}
        {loadError && <Banner status="error" title="加载失败" description={loadError} />}

        <OverviewCards overview={overview} />

        {sync && (sync.running || sync.failed > 0) && <SyncBanner sync={sync} />}

        <VStack gap={3}>
          <HStack gap={2} vAlign="center" hAlign="between" wrap="wrap">
            <Heading level={3}>Key 列表</Heading>
            <HStack gap={2} wrap="wrap">
              <Button label="刷新" variant="ghost" onClick={() => void refresh()} />
              <Button label="导出" variant="secondary" onClick={exportKeys} />
              <Button
                label={syncRunning ? '同步中…' : '同步全部用量'}
                variant="secondary"
                isDisabled={syncRunning}
                onClick={startSync}
              />
              <Button
                label="清理失效 Key"
                variant="secondary"
                isDisabled={!overview?.invalid}
                onClick={() =>
                  setConfirm({
                    title: '清理失效 Key？',
                    description: `将删除 ${overview?.invalid ?? 0} 个被上游判定为无效（401）的 Key。`,
                    actionLabel: '全部删除',
                    run: async () => {
                      const {deleted} = await api.deleteInvalid();
                      toast({body: `已删除 ${deleted} 个失效 Key`});
                      await refresh();
                    },
                  })
                }
              />
              <Button label="导入 Key" variant="primary" onClick={() => setImportOpen(true)} />
            </HStack>
          </HStack>
          <Card padding={0}>
            {keys && keys.length === 0 ? (
              <EmptyState
                title="还没有 Key"
                description="导入 Tavily API Key 后即可开始代理请求。"
                actions={<Button label="导入 Key" variant="primary" onClick={() => setImportOpen(true)} />}
              />
            ) : (
              <Table data={keys ?? []} columns={columns} idKey="id" hasHover verticalAlign="middle" />
            )}
          </Card>
        </VStack>
      </VStack>

      <ImportDialog
        isOpen={importOpen}
        onOpenChange={setImportOpen}
        defaultQuota={defaultQuota}
        onImported={onImported}
      />
      <EditDialog item={editing} onOpenChange={(open) => !open && setEditing(null)} onSaved={replaceKey} />
      <AlertDialog
        isOpen={confirm !== null}
        onOpenChange={(open) => !open && !confirmLoading && setConfirm(null)}
        title={confirm?.title ?? ''}
        description={confirm?.description ?? ''}
        actionLabel={confirm?.actionLabel ?? '确定'}
        cancelLabel="取消"
        isActionLoading={confirmLoading}
        onAction={runConfirm}
      />
    </VStack>
  );
}

function usageVariant(used: number, total: number): 'success' | 'warning' | 'error' {
  if (total <= 0) return 'error';
  const ratio = used / total;
  if (ratio >= 0.95) return 'error';
  if (ratio >= 0.8) return 'warning';
  return 'success';
}

function OverviewCards({overview}: {overview: Overview | null}) {
  const o = overview;
  const stats: {label: string; value: number | undefined; hint?: string}[] = [
    {label: 'Key 总数', value: o?.key_count},
    {label: '可用', value: o?.available},
    {label: '冷却中', value: o?.cooling, hint: '触发 429 后暂时跳过'},
    {label: '已耗尽', value: o?.exhausted},
    {label: '禁用 / 失效', value: o ? o.disabled + o.invalid : undefined},
  ];
  return (
    <VStack gap={4}>
      <Grid columns={{minWidth: 160}} gap={4}>
        {stats.map((s) => (
          <Card key={s.label} padding={4}>
            <VStack gap={1}>
              <Text type="supporting" color="secondary">
                {s.label}
              </Text>
              <Text type="display-3">{s.value === undefined ? '—' : fmt(s.value)}</Text>
              {s.hint && (
                <Text type="supporting" size="sm" color="secondary">
                  {s.hint}
                </Text>
              )}
            </VStack>
          </Card>
        ))}
      </Grid>
      <Card padding={4}>
        <VStack gap={2}>
          <HStack hAlign="between" vAlign="center" wrap="wrap" gap={2}>
            <Text type="body" weight="medium">
              本月额度
            </Text>
            <Text type="supporting" color="secondary">
              {o
                ? `已用 ${fmt(o.used_quota)} / 总计 ${fmt(o.total_quota)}，剩余 ${fmt(o.remaining_quota)}`
                : '加载中…'}
            </Text>
          </HStack>
          <ProgressBar
            label="本月额度使用"
            isLabelHidden
            value={o ? Math.min(o.used_quota, o.total_quota) : 0}
            max={Math.max(o?.total_quota ?? 1, 1)}
            variant={o ? usageVariant(o.used_quota, o.total_quota) : 'success'}
          />
        </VStack>
      </Card>
    </VStack>
  );
}

function SyncBanner({sync}: {sync: SyncStatus}) {
  if (sync.running) {
    return (
      <Banner status="info" title={`正在同步用量：${sync.done} / ${sync.total}`} collapsible={false}>
        <ProgressBar label="同步进度" isLabelHidden value={sync.done} max={Math.max(sync.total, 1)} />
      </Banner>
    );
  }
  return (
    <Banner
      status="warning"
      title={`上次同步有 ${sync.failed} 个 Key 失败${sync.skipped ? `，${sync.skipped} 个因 Tavily 限流未同步` : ''}`}
      description={[
        sync.finished_at && `完成于 ${new Date(sync.finished_at).toLocaleString('zh-CN')}`,
        sync.skipped > 0 && 'Tavily 的 /usage 限流对所有 Key 共享，请稍后再同步。',
      ]
        .filter(Boolean)
        .join('。') || undefined}
      isDismissable
      collapsible={{defaultIsOpen: true}}
    >
      <VStack gap={1}>
        {sync.errors.slice(0, 20).map((e) => (
          <Text key={e.id} type="body" size="sm">
            {e.alias}：{e.error}
          </Text>
        ))}
      </VStack>
    </Banner>
  );
}
