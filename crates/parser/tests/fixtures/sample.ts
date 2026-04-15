import { Injectable } from '@nestjs/common';
import { Repository } from 'typeorm';

interface Identifiable {
  id: number;
}

type UserRole = 'admin' | 'user' | 'guest';

enum Status {
  Active = 'active',
  Inactive = 'inactive',
}

export class UserService {
  constructor(private readonly repo: Repository<any>) {}

  async findAll(): Promise<any[]> {
    return this.repo.find();
  }

  async findById(id: number): Promise<any> {
    return this.repo.findOne({ where: { id } });
  }

  async delete(id: number): Promise<void> {
    await this.repo.delete(id);
  }
}

export const DEFAULT_TIMEOUT = 5000;

function createLogger(name: string) {
  return { log: (msg: string) => console.log(`[${name}] ${msg}`) };
}
