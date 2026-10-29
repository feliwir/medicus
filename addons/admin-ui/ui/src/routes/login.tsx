import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Navigate, useNavigate } from "@tanstack/react-router";
import { AlertCircle } from "lucide-react";
import { type FormEvent, useState } from "react";
import { Alert } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { api } from "@/lib/api";

/** A centred page with the logo, for login and setup. */
export function Centered({ children, wide = false }: { children: React.ReactNode; wide?: boolean }) {
  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background p-4">
      <div className="flex items-center gap-3 text-title-lg font-semibold tracking-tight">
        <img src="/favicon.svg" alt="" className="size-9" />
        medicus
      </div>
      <div className={wide ? "w-full max-w-2xl" : "w-full max-w-md"}>{children}</div>
    </div>
  );
}

export function Login() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const setup = useQuery({ queryKey: ["setup"], queryFn: api.setupStatus });
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const login = useMutation({
    mutationFn: () => api.login(name, password),
    onSuccess: async () => {
      await queryClient.invalidateQueries();
      void navigate({ to: "/" });
    },
  });

  if (setup.data?.required) return <Navigate to="/setup" />;

  const submit = (e: FormEvent) => {
    e.preventDefault();
    login.mutate();
  };
  return (
    <Centered>
      <Card>
        <CardHeader>
          <CardTitle className="text-headline-sm font-normal">Sign in</CardTitle>
          <CardDescription>With your medicus account.</CardDescription>
        </CardHeader>
        <CardContent>
          <form className="space-y-4" onSubmit={submit}>
            <div className="space-y-2">
              <Label htmlFor="name">User name</Label>
              <Input id="name" autoComplete="username" autoFocus value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label htmlFor="password">Password</Label>
              <Input
                id="password"
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </div>
            {login.error && (
              <Alert variant="destructive">
                <AlertCircle />
                {login.error.message}
              </Alert>
            )}
            <Button type="submit" className="mt-2 w-full" disabled={login.isPending || !name || !password}>
              {login.isPending ? "Signing in…" : "Sign in"}
            </Button>
          </form>
        </CardContent>
      </Card>
    </Centered>
  );
}
