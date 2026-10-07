import {useEffect, useState} from 'react';
import {Banner} from '@astryxdesign/core/Banner';
import {Button} from '@astryxdesign/core/Button';
import {Dialog, DialogHeader} from '@astryxdesign/core/Dialog';
import {HStack, VStack} from '@astryxdesign/core/Layout';
import {NumberInput} from '@astryxdesign/core/NumberInput';
import {Text} from '@astryxdesign/core/Text';
import {TextArea} from '@astryxdesign/core/TextArea';
import {TextInput} from '@astryxdesign/core/TextInput';
import {api, errorMessage, type ImportResult, type KeyItem} from './api';

export function ImportDialog({
  isOpen,
  onOpenChange,
  defaultQuota,
  onImported,
}: {
  isOpen: boolean;
  onOpenChange: (open: boolean) => void;
  defaultQuota: number;
  onImported: (result: ImportResult) => void;
}) {
  const [text, setText] = useState('');
  const [quota, setQuota] = useState<number | null>(defaultQuota);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [result, setResult] = useState<ImportResult | null>(null);

  useEffect(() => {
    if (isOpen) {
      setText('');
      setQuota(defaultQuota);
      setError('');
      setResult(null);
    }
  }, [isOpen, defaultQuota]);

  // The dialog stays mounted while closed, and the pasted text holds raw keys; drop it in the
  // same render that closes the dialog.
  const changeOpen = (open: boolean) => {
    if (!open) {
      setText('');
      setResult(null);
    }
    onOpenChange(open);
  };

  const submit = async () => {
    setError('');
    if (!text.trim()) {
      setError('请至少输入一个 Key');
      return;
    }
    setLoading(true);
    try {
      const r = await api.importKeys(text, quota && quota > 0 ? quota : undefined);
      setResult(r);
      onImported(r);
      if (r.errors.length === 0) changeOpen(false);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  };

  return (
    <Dialog isOpen={isOpen} onOpenChange={changeOpen} purpose="form" width={600}>
      <DialogHeader
        title="导入 Key"
        subtitle="每行一个：key、key,别名 或 key,别名,总额度。# 开头的行会被忽略。"
        onOpenChange={changeOpen}
      />
      <VStack gap={4} padding={4} hAlign="stretch">
        {error && <Banner status="error" title={error} />}
        {result && result.errors.length > 0 && (
          <Banner
            status="warning"
            title={`已导入 ${result.added} 个，重复 ${result.duplicates} 个，${result.errors.length} 行有误`}
            collapsible={false}
          >
            <VStack gap={1}>
              {result.errors.slice(0, 20).map((e) => (
                <Text key={e.line} type="body" size="sm">
                  第 {e.line} 行：{e.reason}
                </Text>
              ))}
            </VStack>
          </Banner>
        )}
        <TextArea
          label="Key 列表"
          value={text}
          onChange={setText}
          placeholder={'tvly-xxxxxxxx\ntvly-yyyyyyyy,备用账号,1000'}
          rows={10}
        />
        <NumberInput
          label="默认总额度"
          description="未在行内指定额度的 Key 使用此值"
          value={quota}
          onChange={setQuota}
          min={1}
          isIntegerOnly
        />
        <HStack gap={2} hAlign="end">
          <Button label="取消" variant="ghost" onClick={() => changeOpen(false)} />
          <Button label="导入" variant="primary" isLoading={loading} onClick={submit} />
        </HStack>
      </VStack>
    </Dialog>
  );
}

export function EditDialog({
  item,
  onOpenChange,
  onSaved,
}: {
  item: KeyItem | null;
  onOpenChange: (open: boolean) => void;
  onSaved: (item: KeyItem) => void;
}) {
  const [alias, setAlias] = useState('');
  const [total, setTotal] = useState<number | null>(null);
  const [used, setUsed] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    if (item) {
      setAlias(item.alias);
      setTotal(item.total_quota);
      setUsed(item.used_quota);
      setError('');
    }
  }, [item]);

  const submit = async () => {
    if (!item) return;
    setError('');
    if (!total || total <= 0) {
      setError('总额度必须大于 0');
      return;
    }
    setLoading(true);
    try {
      const saved = await api.updateKey(item.id, {
        alias: alias.trim(),
        total_quota: total,
        used_quota: Math.max(0, used ?? 0),
      });
      onSaved(saved);
      onOpenChange(false);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  };

  return (
    <Dialog isOpen={item !== null} onOpenChange={onOpenChange} purpose="form" width={480}>
      <DialogHeader title="编辑 Key" subtitle={item?.key} onOpenChange={onOpenChange} />
      <VStack gap={4} padding={4} hAlign="stretch">
        {error && <Banner status="error" title={error} />}
        <TextInput label="别名" value={alias} onChange={setAlias} />
        <NumberInput label="总额度" value={total} onChange={setTotal} min={1} isIntegerOnly />
        <NumberInput
          label="已用额度"
          description="本月已消耗的请求数，可手动校正"
          value={used}
          onChange={setUsed}
          min={0}
          isIntegerOnly
        />
        <HStack gap={2} hAlign="end">
          <Button label="取消" variant="ghost" onClick={() => onOpenChange(false)} />
          <Button label="保存" variant="primary" isLoading={loading} onClick={submit} />
        </HStack>
      </VStack>
    </Dialog>
  );
}
