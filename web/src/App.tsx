import {useEffect, useState} from 'react';
import {LOGOUT_EVENT, clearToken, getToken, setToken} from './api';
import {Dashboard} from './Dashboard';
import {Login} from './Login';

export function App() {
  const [token, setTokenState] = useState<string | null>(() => getToken());

  useEffect(() => {
    const onLogout = () => setTokenState(null);
    window.addEventListener(LOGOUT_EVENT, onLogout);
    return () => window.removeEventListener(LOGOUT_EVENT, onLogout);
  }, []);

  if (!token) {
    return (
      <Login
        onLogin={(t) => {
          setToken(t);
          setTokenState(t);
        }}
      />
    );
  }

  return (
    <Dashboard
      onLogout={() => {
        clearToken();
        setTokenState(null);
      }}
    />
  );
}
