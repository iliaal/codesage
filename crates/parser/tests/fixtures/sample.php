<?php

namespace App\Http\Controllers;

interface Loggable
{
    public function log(string $message): void;
}

trait Cacheable
{
    public function cacheKey(): string
    {
        return static::class;
    }
}

class UserController
{
    use Cacheable;

    const MAX_USERS = 100;

    public function index(): array
    {
        return [];
    }

    public function show(int $id): array
    {
        return ['id' => $id];
    }

    public function calls(UserController $other): void
    {
        $this->show(1);
        $other?->index();
        self::show(2);
    }
}

enum Status: string
{
    case Active = 'active';
    case Inactive = 'inactive';
}

function helper(): void
{
}
