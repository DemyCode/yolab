// The rjsf registries live here rather than beside the components, because a
// file that exports both components and plain objects breaks Fast Refresh.
import {
  ArrayFieldTemplate,
  FieldTemplate,
  ObjectFieldTemplate,
} from "./templates";
import {
  CheckboxWidget,
  PasswordWidget,
  TextareaWidget,
  TunnelWidget,
} from "./widgets";

export const templates = {
  FieldTemplate,
  ObjectFieldTemplate,
  ArrayFieldTemplate,
};

export const widgets = {
  TunnelWidget,
  PasswordWidget,
  CheckboxWidget,
  TextareaWidget,
};
