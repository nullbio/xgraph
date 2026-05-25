<?php

namespace App\Services;

use App\Contracts\OrderContract;
use App\Support\Loggable;
use App\Services\BaseService;

class OrderService extends BaseService implements OrderContract
{
    use Loggable;

    public const STATUS_PENDING = 'pending';

    private OrderRepository $repository;

    public function __construct(OrderRepository $repository)
    {
        $this->repository = $repository;
    }

    public function place(int $customerId): bool
    {
        return $this->repository->store($customerId);
    }
}
