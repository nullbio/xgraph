import { Logger } from 'logger';

export interface Person {
    name: string;
    age: number;
}

export type Maybe<T> = T | null;

export function greet(person: Person): Maybe<string> {
    return `Hello, ${person.name}`;
}

export class Service<T> {
    private items: Array<T> = [];

    add(item: T): void {
        this.items.push(item);
    }

    log(message: string): void {
        new Logger().info(message);
    }
}

const make = (): Service<number> => new Service<number>();
