import { useState } from 'react';
import { Button } from './ui/Button';

interface Props {
    label: string;
}

export function Greeting({ label }: Props): JSX.Element {
    const [count, setCount] = useState<number>(0);
    return (
        <div className="greeting">
            <Button onClick={() => setCount(count + 1)}>{label}</Button>
        </div>
    );
}
