import { ConnectionField } from "./fields";
import {
  ArrayFieldTemplate,
  FieldTemplate,
  ObjectFieldTemplate,
} from "./templates";
import {
  CheckboxWidget,
  FolderWidget,
  PasswordWidget,
  ServiceUrlWidget,
  TextareaWidget,
  TunnelWidget,
  YolabTokenWidget,
} from "./widgets";

export const templates = {
  FieldTemplate,
  ObjectFieldTemplate,
  ArrayFieldTemplate,
};

export const fields = {
  ConnectionField,
};

export const widgets = {
  TunnelWidget,
  YolabTokenWidget,
  ServiceUrlWidget,
  FolderWidget,
  PasswordWidget,
  CheckboxWidget,
  TextareaWidget,
};
