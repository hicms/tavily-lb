import {useState, type CSSProperties} from 'react';
import {Banner} from '@astryxdesign/core/Banner';
import {Button} from '@astryxdesign/core/Button';
import {Card} from '@astryxdesign/core/Card';
import {Center} from '@astryxdesign/core/Center';
import {VStack} from '@astryxdesign/core/Layout';
import {Heading, Text} from '@astryxdesign/core/Text';
import {TextInput} from '@astryxdesign/core/TextInput';
import {api, errorMessage} from './api';

const pageStyle: CSSProperties = {
  minHeight: '100%',
  backgroundColor: 'var(--color-background-body)',
};

const columnStyle: CSSProperties = {width: '100%', maxWidth: 420};

export function Login({onLogin}: {onLogin: (token: string) => void}) {
  const [token, setToken] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  const submit = async () => {
    const value = token.trim();
    setError('');
    if (!value) {
      setError('请输入 Master Key');
      return;
    }
    setLoading(true);
    try {
      await api.verify(value);
      onLogin(value);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  };

  return (
    <Center axis="both" padding={6} style={pageStyle}>
      <VStack gap={4} hAlign="center" style={columnStyle}>
        <VStack gap={1} hAlign="center">
          <Text type="body" weight="bold" size="lg">
            Tavily LB
          </Text>
          <Text type="body" color="secondary" size="sm">
            Tavily API Key 负载均衡器
          </Text>
        </VStack>
        <Card padding={8} width="100%">
          <VStack gap={4} hAlign="stretch">
            <VStack gap={1} hAlign="center">
              <Heading level={2}>登录管理后台</Heading>
              <Text type="body" color="secondary" size="sm">
                使用 Master Key 登录。首次启动时自动生成的 Key 会打印在服务日志中。
              </Text>
            </VStack>
            {error && <Banner status="error" title={error} container="card" />}
            <TextInput
              label="Master Key"
              type="password"
              size="lg"
              value={token}
              onChange={setToken}
              onEnter={submit}
              placeholder="粘贴 Master Key"
              hasAutoFocus
            />
            <Button label="登录" variant="primary" size="lg" isLoading={loading} onClick={submit} />
          </VStack>
        </Card>
      </VStack>
    </Center>
  );
}
