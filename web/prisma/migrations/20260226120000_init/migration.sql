-- CreateTable
CREATE TABLE "User" (
    "id" TEXT NOT NULL PRIMARY KEY,
    "username" TEXT NOT NULL,
    "email" TEXT NOT NULL,
    "passwordHash" TEXT NOT NULL,
    "createdAt" DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" DATETIME NOT NULL
);

-- CreateTable
CREATE TABLE "Session" (
    "id" TEXT NOT NULL PRIMARY KEY,
    "userId" TEXT NOT NULL,
    "expiresAt" DATETIME NOT NULL,
    CONSTRAINT "Session_userId_fkey" FOREIGN KEY ("userId") REFERENCES "User" ("id") ON DELETE CASCADE ON UPDATE CASCADE
);

-- CreateTable
CREATE TABLE "Simulation" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "ownerId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "description" TEXT,
    "createdAt" DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" DATETIME NOT NULL,
    CONSTRAINT "Simulation_ownerId_fkey" FOREIGN KEY ("ownerId") REFERENCES "User" ("id") ON DELETE CASCADE ON UPDATE CASCADE
);

-- CreateTable
CREATE TABLE "SimulationNode" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "simulationId" INTEGER NOT NULL,
    "uid" TEXT NOT NULL,
    "type" TEXT NOT NULL,
    "nodeId" INTEGER,
    "label" TEXT NOT NULL,
    "x" REAL NOT NULL,
    "y" REAL NOT NULL,
    "roleHint" TEXT,
    "encBufferSize" INTEGER,
    "decBufferSize" INTEGER,
    "keyGenerationRate" REAL,
    CONSTRAINT "SimulationNode_simulationId_fkey" FOREIGN KEY ("simulationId") REFERENCES "Simulation" ("id") ON DELETE CASCADE ON UPDATE CASCADE
);

-- CreateTable
CREATE TABLE "SimulationLink" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "simulationId" INTEGER NOT NULL,
    "uid" TEXT NOT NULL,
    "sourceUid" TEXT NOT NULL,
    "targetUid" TEXT NOT NULL,
    "linkType" TEXT NOT NULL,
    "rIj" REAL,
    "latencyMs" INTEGER,
    CONSTRAINT "SimulationLink_simulationId_fkey" FOREIGN KEY ("simulationId") REFERENCES "Simulation" ("id") ON DELETE CASCADE ON UPDATE CASCADE
);

-- CreateTable
CREATE TABLE "SimulationRun" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "simulationId" INTEGER NOT NULL,
    "status" TEXT NOT NULL,
    "message" TEXT,
    "queuedAt" DATETIME,
    "startedAt" DATETIME,
    "finishedAt" DATETIME,
    "createdAt" DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "SimulationRun_simulationId_fkey" FOREIGN KEY ("simulationId") REFERENCES "Simulation" ("id") ON DELETE CASCADE ON UPDATE CASCADE
);

-- CreateIndex
CREATE UNIQUE INDEX "User_username_key" ON "User"("username");

-- CreateIndex
CREATE UNIQUE INDEX "User_email_key" ON "User"("email");

-- CreateIndex
CREATE INDEX "Session_userId_idx" ON "Session"("userId");

-- CreateIndex
CREATE INDEX "Session_expiresAt_idx" ON "Session"("expiresAt");

-- CreateIndex
CREATE INDEX "Simulation_ownerId_idx" ON "Simulation"("ownerId");

-- CreateIndex
CREATE INDEX "Simulation_updatedAt_idx" ON "Simulation"("updatedAt");

-- CreateIndex
CREATE UNIQUE INDEX "SimulationNode_simulationId_uid_key" ON "SimulationNode"("simulationId", "uid");

-- CreateIndex
CREATE UNIQUE INDEX "SimulationNode_simulationId_nodeId_key" ON "SimulationNode"("simulationId", "nodeId");

-- CreateIndex
CREATE INDEX "SimulationNode_simulationId_idx" ON "SimulationNode"("simulationId");

-- CreateIndex
CREATE UNIQUE INDEX "SimulationLink_simulationId_uid_key" ON "SimulationLink"("simulationId", "uid");

-- CreateIndex
CREATE INDEX "SimulationLink_simulationId_idx" ON "SimulationLink"("simulationId");

-- CreateIndex
CREATE INDEX "SimulationLink_sourceUid_idx" ON "SimulationLink"("sourceUid");

-- CreateIndex
CREATE INDEX "SimulationLink_targetUid_idx" ON "SimulationLink"("targetUid");

-- CreateIndex
CREATE INDEX "SimulationRun_simulationId_idx" ON "SimulationRun"("simulationId");

-- CreateIndex
CREATE INDEX "SimulationRun_status_idx" ON "SimulationRun"("status");

-- CreateIndex
CREATE INDEX "SimulationRun_createdAt_idx" ON "SimulationRun"("createdAt");
