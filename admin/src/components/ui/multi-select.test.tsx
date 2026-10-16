import { describe, expect, it, vi } from "vitest"
import { render, screen } from "@testing-library/react"
import { MultiSelect } from "./multi-select"

// Regression test for feedback item #1 (Xgateway v3.4.3):
// "角色授权实例窗口显示不全" — the trigger's flex child needs `min-w-0` so long
// selected-option chips shrink instead of pushing the "+ 授权" button out of view.
describe("MultiSelect", () => {
  const options = [
    { value: 1, label: "zhangasn" },
    { value: 2, label: "智能路由测试2.5" },
    { value: 3, label: "模型测试01" },
  ]

  it("keeps the chip row shrinkable so the trigger never overflows its container", () => {
    render(
      <MultiSelect options={options} value={[1, 2, 3]} onChange={vi.fn()} />
    )

    const combobox = screen.getByRole("combobox")
    const chipRow = combobox.firstElementChild as HTMLElement

    expect(chipRow).toBeTruthy()
    expect(chipRow.className).toContain("min-w-0")
  })

  it("renders a chip per selected option with a remove control", () => {
    render(
      <MultiSelect options={options} value={[1, 2]} onChange={vi.fn()} />
    )

    expect(screen.getByText("zhangasn")).toBeInTheDocument()
    expect(screen.getByText("智能路由测试2.5")).toBeInTheDocument()
    expect(screen.queryByText("模型测试01")).not.toBeInTheDocument()
  })

  it("shows the placeholder when nothing is selected", () => {
    render(
      <MultiSelect options={options} value={[]} onChange={vi.fn()} placeholder="Select options..." />
    )

    expect(screen.getByText("Select options...")).toBeInTheDocument()
  })
})
